#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

//! The Lamella WASM tier's DRIVER BRIDGE: a `lamella_i2c` import world whose host side is the
//! CIL INTERPRETER running the real, unmodified layer-1 C# driver stack -- so a wasm guest
//! reads a peripheral through the very driver a C# program uses, with no sensor logic ported
//! by hand. **THIS TIER IS EXPERIMENTAL AND SHIPS AS A PREVIEW:** one bus is granted here
//! today, the scope is expected to expand well beyond that, and the API may change in any
//! release -- treat it as provisional rather than settled. A guest's bus transactions cross
//! the boundary as `(ptr, len)` pairs -- copied in, copied out, with nothing aliased and no
//! host pointer ever entering the module -- become managed `byte[]` contents inside a harness
//! class, and dispatch VIRTUALLY through the abstract `System.Device.I2c.I2cDriver` seam. So
//! the concrete driver swaps -- a simulated device on a host, the board's real controller on
//! silicon -- without the bridge, the harness, or the guest changing.

extern crate alloc;

use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;

use lamella_cil_runtime::{run, MethodId, Module, Value as CilValue, Vm};
use lamella_load::load_with_corlib_and_libraries;
use lamella_metadata::Assembly;

/// The lifetime a session's PE bytes must satisfy. Under the execute-in-place levels the
/// loader BORROWS method bodies out of the image rather than copying them, so the bytes have
/// to outlive the session: the alias becomes `&'static [u8]` and the bound is checked at the
/// call site. On a device the assemblies are `include_bytes!` statics in flash, so the
/// stricter form costs the caller nothing; a host caller leaks its fixture instead.
#[cfg(not(feature = "flash-image"))]
type PeBytes<'pe> = &'pe [u8];
#[cfg(feature = "flash-image")]
type PeBytes<'pe> = &'static [u8];
use lamella_wasm_interp::{FuncType, HostFunc, Trap, ValType, Value as WasmValue, World};

/// The harness's fixed transfer-buffer capacity (BridgeHarness.cs allocates 32-byte
/// buffers); a longer transfer is a bridge-contract violation, refused loudly.
const BUFFER_CAPACITY: u32 = 32;

/// The one granted bus handle: pre-opened and pre-configured, never opened by a guest.
const BUS: u32 = 0;

/// The one readiness source this world grants: the accelerometer's data-ready.
const EVT_SOURCE_DATA_READY: u32 = 1;
/// The subscription handle handed back for that source (single-slot, so a single id).
const EVT_SUBSCRIPTION: u32 = 1;
/// A refused subscribe -- reported as data, never as a trap.
const EVT_NO_SUCH_SOURCE: u32 = 0;
/// `wait_ready` bounded out without an edge landing (the simulated world's same value).
const EVT_TIMED_OUT: u32 = 1;

/// Why the bridge could not open its interpreter session.
#[derive(Debug)]
pub enum BridgeError {
    /// An assembly's metadata did not parse; the string names which.
    Metadata(&'static str),
    /// The loader refused the assembly set.
    Load(String),
    /// The harness's `Main` (the one-time setup) trapped or returned the wrong sentinel.
    Setup(String),
    /// A harness method the bridge binds to is missing; the string names it.
    MissingMethod(&'static str),
}

/// A live interpreter session around the harness: the loaded module, the persistent [`Vm`]
/// (heap + statics survive across bridged calls), and the bound harness methods.
pub struct DriverSession {
    module: Module,
    vm: Vm,
    set_write: MethodId,
    get_read: MethodId,
    write: MethodId,
    read: MethodId,
    write_read: MethodId,
    probe: MethodId,
    /// The readiness half, bound only if the harness carries it -- an i2c-only harness does
    /// not, so a session without it stays legal and [`world`] is unaffected.
    enable_data_ready: Option<MethodId>,
    data_ready: Option<MethodId>,
}

impl DriverSession {
    /// Loads corlib + the library assemblies + the harness program, runs the harness's
    /// `Main` once (it constructs the driver behind the abstract seam and must return 42),
    /// and binds the accessor methods.
    pub fn open<'pe>(
        corlib: PeBytes<'pe>,
        libraries: &[PeBytes<'pe>],
        harness: PeBytes<'pe>,
    ) -> Result<DriverSession, BridgeError> {
        DriverSession::open_with(corlib, libraries, harness, |_| {})
    }

    /// [`open`](Self::open), with a hook that configures the fresh [`Vm`] BEFORE the
    /// harness's `Main` runs -- an on-device host installs the board's volatile MMIO seam here
    /// (`Vm::set_mmio`), since `Main`'s `Configure` reaches real registers through it. A host
    /// running without hardware passes a no-op.
    pub fn open_with<'pe>(
        corlib: PeBytes<'pe>,
        libraries: &[PeBytes<'pe>],
        harness: PeBytes<'pe>,
        configure_vm: impl FnOnce(&mut Vm),
    ) -> Result<DriverSession, BridgeError> {
        let corlib = Assembly::read(corlib).map_err(|_| BridgeError::Metadata("corlib"))?;
        let mut libs = Vec::new();
        for bytes in libraries {
            libs.push(Assembly::read(bytes).map_err(|_| BridgeError::Metadata("library"))?);
        }
        let harness = Assembly::read(harness).map_err(|_| BridgeError::Metadata("harness"))?;
        let program = load_with_corlib_and_libraries(&corlib, &libs, &harness)
            .map_err(|e| BridgeError::Load(alloc::format!("{e:?}")))?;

        let mut vm = Vm::new();
        configure_vm(&mut vm);
        DriverSession::bind(program.module, program.entry, vm)
    }

    /// Boots a session from a host-BAKED image rather than loading PEs at run time: the
    /// module's tables are borrowed straight out of the flash-resident bytes, so the whole
    /// binding layer costs no arena. On a part whose SRAM cannot hold the loader's tables
    /// this is the only shape that boots at all -- an assembly set large enough to exhaust the
    /// arena while its bodies are left in place is resident-free once baked.
    ///
    /// `configure_vm` runs before the harness's `Main`, exactly as in
    /// [`open_with`](Self::open_with).
    ///
    /// # Errors
    /// [`BridgeError`] if the image will not boot (not an image, a version the runtime does
    /// not speak, or an intrinsic this build lacks), if `Main` misbehaves, or if a bound
    /// accessor is absent -- note a reachability-TRIMMED image can bake the accessors away,
    /// since they are reached from the host, never from CIL.
    #[cfg(feature = "code-in-place")]
    pub fn open_baked(
        image: &'static [u8],
        configure_vm: impl FnOnce(&mut Vm),
    ) -> Result<DriverSession, BridgeError> {
        let (module, entry) =
            Module::from_baked(image).map_err(|e| BridgeError::Load(alloc::format!("{e:?}")))?;
        let entry =
            entry.ok_or_else(|| BridgeError::Setup(String::from("image records no entry point")))?;
        let mut vm = Vm::new();
        configure_vm(&mut vm);
        let entry = lamella_cil_runtime::boot_baked(&module, &mut vm, entry)
            .map_err(|trap| BridgeError::Setup(alloc::format!("static constructor: {trap}")))?;
        DriverSession::bind(module, entry, vm)
    }

    /// Runs the harness's one-time `Main` on an already-configured [`Vm`] (it must return the
    /// 42 sentinel), then binds the accessors -- the tail both openers share.
    fn bind(module: Module, entry: MethodId, mut vm: Vm) -> Result<DriverSession, BridgeError> {
        match run(&module, &mut vm, entry, Vec::new()) {
            Ok(Some(CilValue::Int32(42))) => {}
            Ok(other) => {
                return Err(BridgeError::Setup(alloc::format!(
                    "harness Main returned {other:?}, expected 42"
                )));
            }
            Err(trap) => {
                return Err(BridgeError::Setup(alloc::format!("harness Main trapped: {trap}")));
            }
        }

        let find = |name: &'static str| -> Result<MethodId, BridgeError> {
            find_method(&module, name).ok_or(BridgeError::MissingMethod(name))
        };
        Ok(DriverSession {
            set_write: find("BridgeHarness.SetWrite")?,
            get_read: find("BridgeHarness.GetRead")?,
            write: find("BridgeHarness.Write")?,
            read: find("BridgeHarness.Read")?,
            write_read: find("BridgeHarness.WriteRead")?,
            probe: find("BridgeHarness.Probe")?,
            enable_data_ready: find_method(&module, "BridgeHarness.EnableDataReady"),
            data_ready: find_method(&module, "BridgeHarness.DataReady"),
            module,
            vm,
        })
    }

    /// One interpreted static call returning an int. An escaping trap is a HOST failure
    /// -- the guest never sees it as a status.
    fn call_i32(&mut self, method: MethodId, args: Vec<CilValue>) -> Result<i32, Trap> {
        match run(&self.module, &mut self.vm, method, args) {
            Ok(Some(CilValue::Int32(v))) => Ok(v),
            Ok(_) => Err(Trap::Host("interpreted harness returned a non-int")),
            Err(_) => Err(Trap::Host("interpreted driver trapped")),
        }
    }

    /// Copies a guest range into the harness write buffer, one accessor call per byte.
    fn stage_write(&mut self, mem: &[u8], start: usize, len: usize) -> Result<(), Trap> {
        for i in 0..len {
            let byte = i32::from(mem[start + i]);
            self.call_i32(self.set_write, vec![CilValue::Int32(i as i32), CilValue::Int32(byte)])?;
        }
        Ok(())
    }

    /// Copies the harness read buffer back into a guest range.
    fn unstage_read(&mut self, mem: &mut [u8], start: usize, len: usize) -> Result<(), Trap> {
        for i in 0..len {
            let v = self.call_i32(self.get_read, vec![CilValue::Int32(i as i32)])?;
            mem[start + i] = v as u8;
        }
        Ok(())
    }

    /// One bus transaction driven from the HOST rather than from a guest: the same staging,
    /// the same harness accessors, the same interpreted driver the granted world uses -- only
    /// the bytes come from Rust slices instead of wasm linear memory. An empty `read` is a
    /// plain write; otherwise it is the write-then-read (repeated start) primitive.
    ///
    /// This exists so a firmware image can put the raw wire bytes on the UART -- a guest
    /// reports one `i32`, which is too narrow to diagnose a six-byte frame. Because it shares
    /// the staging path rather than duplicating it, what it prints is what a guest would have
    /// received.
    ///
    /// Returns the layer-1 status verbatim (0 Ok / 1 AddressNack / 2 DataNack / 3 other);
    /// `read` is left untouched unless the status is Ok.
    ///
    /// # Errors
    /// [`Trap`] if either buffer exceeds the harness capacity, or if the interpreted driver
    /// trapped (a HOST failure -- never a status).
    pub fn transact(
        &mut self,
        address: u32,
        write: &[u8],
        read: &mut [u8],
    ) -> Result<i32, Trap> {
        if write.len() as u32 > BUFFER_CAPACITY || read.len() as u32 > BUFFER_CAPACITY {
            return Err(Trap::Host("bridge: transfer exceeds the harness buffer"));
        }
        self.stage_write(write, 0, write.len())?;
        let (method, args) = if read.is_empty() {
            (self.write, vec![CilValue::Int32(address as i32), CilValue::Int32(write.len() as i32)])
        } else {
            (
                self.write_read,
                vec![
                    CilValue::Int32(address as i32),
                    CilValue::Int32(write.len() as i32),
                    CilValue::Int32(read.len() as i32),
                ],
            )
        };
        let status = self.call_i32(method, args)?;
        if status == 0 && !read.is_empty() {
            let len = read.len();
            self.unstage_read(read, 0, len)?;
        }
        Ok(status)
    }
}

/// Resolves a loader-recorded qualified method name (`Type.Method`) to its id -- the same
/// declared-identity scan the REPL session uses.
fn find_method(module: &Module, name: &str) -> Option<MethodId> {
    reflected_method(module, name).or_else(|| {
        let mut id: MethodId = 0;
        while module.method_kind(id).is_some() {
            if module.method_name(id) == Some(name) {
                return Some(id);
            }
            id += 1;
        }
        None
    })
}

/// Resolves `Type.Method` through the REFLECTION tables (type name -> handle -> member handle
/// -> id) rather than the debugger-name scan. This is the path that survives baking: the
/// scan reads `method_debug`, which a baked image deliberately does not carry (it is
/// host-only), so a baked session would otherwise find none of its accessors -- while the
/// by-name reflection indexes ARE baked, and resolve against the flash-borrowed tables.
fn reflected_method(module: &Module, name: &str) -> Option<MethodId> {
    let (type_name, member) = name.rsplit_once('.')?;
    let type_handle = module.type_handle_by_name(type_name)?;
    let method_handle = module.type_method_handle(type_handle, member)?;
    module.resolve_by_handle(method_handle)
}

fn i2c_type(params: usize) -> FuncType {
    FuncType { params: vec![ValType::I32; params], results: vec![ValType::I32] }
}

fn arg_i32(args: &[WasmValue], i: usize) -> Result<u32, Trap> {
    match args.get(i) {
        Some(WasmValue::I32(v)) => Ok(*v),
        _ => Err(Trap::Host("bridge: argument type")),
    }
}

/// Bounds-checks a guest `(ptr, len)` pair (an out-of-range buffer is a trap) and
/// the harness capacity (a bridge-contract limit, also loud).
fn guest_range(mem: &[u8], ptr: u32, len: u32) -> Result<(usize, usize), Trap> {
    if len > BUFFER_CAPACITY {
        return Err(Trap::Host("bridge: transfer exceeds the harness buffer"));
    }
    let start = ptr as u64;
    let end = start + u64::from(len);
    if end > mem.len() as u64 {
        return Err(Trap::MemOutOfBounds);
    }
    Ok((start as usize, len as usize))
}

fn check_bus(bus: u32) -> Result<(), Trap> {
    if bus == BUS { Ok(()) } else { Err(Trap::Host("bridge: unknown bus handle")) }
}

/// Builds the granted `lamella_i2c` world over the session: the same four names and types
/// as the simulated-device world, served by the interpreted driver instead.
#[must_use]
pub fn world(session: DriverSession) -> World {
    world_over(&Rc::new(RefCell::new(session)))
}

/// The `lamella_i2c` half over an already-shared session, so the eventful grant can serve
/// both halves from ONE interpreter session -- one device, one host.
fn world_over(session: &Rc<RefCell<DriverSession>>) -> World {
    let session = Rc::clone(session);

    let s = Rc::clone(&session);
    let write = move |mem: &mut [u8], args: &[WasmValue]| -> Result<Option<WasmValue>, Trap> {
        check_bus(arg_i32(args, 0)?)?;
        let addr = arg_i32(args, 1)?;
        let (start, len) = guest_range(mem, arg_i32(args, 2)?, arg_i32(args, 3)?)?;
        let mut s = s.borrow_mut();
        s.stage_write(mem, start, len)?;
        let method = s.write;
        let rc = s.call_i32(
            method,
            vec![CilValue::Int32(addr as i32), CilValue::Int32(len as i32)],
        )?;
        Ok(Some(WasmValue::I32(rc as u32)))
    };

    let s = Rc::clone(&session);
    let read = move |mem: &mut [u8], args: &[WasmValue]| -> Result<Option<WasmValue>, Trap> {
        check_bus(arg_i32(args, 0)?)?;
        let addr = arg_i32(args, 1)?;
        let (start, len) = guest_range(mem, arg_i32(args, 2)?, arg_i32(args, 3)?)?;
        let mut s = s.borrow_mut();
        let method = s.read;
        let rc = s.call_i32(
            method,
            vec![CilValue::Int32(addr as i32), CilValue::Int32(len as i32)],
        )?;
        if rc == 0 {
            s.unstage_read(mem, start, len)?;
        }
        Ok(Some(WasmValue::I32(rc as u32)))
    };

    let s = Rc::clone(&session);
    let write_read = move |mem: &mut [u8], args: &[WasmValue]| -> Result<Option<WasmValue>, Trap> {
        check_bus(arg_i32(args, 0)?)?;
        let addr = arg_i32(args, 1)?;
        let (wstart, wlen) = guest_range(mem, arg_i32(args, 2)?, arg_i32(args, 3)?)?;
        let (rstart, rlen) = guest_range(mem, arg_i32(args, 4)?, arg_i32(args, 5)?)?;
        let mut s = s.borrow_mut();
        s.stage_write(mem, wstart, wlen)?;
        let method = s.write_read;
        let rc = s.call_i32(
            method,
            vec![
                CilValue::Int32(addr as i32),
                CilValue::Int32(wlen as i32),
                CilValue::Int32(rlen as i32),
            ],
        )?;
        if rc == 0 {
            s.unstage_read(mem, rstart, rlen)?;
        }
        Ok(Some(WasmValue::I32(rc as u32)))
    };

    let s = Rc::clone(&session);
    let probe = move |_mem: &mut [u8], args: &[WasmValue]| -> Result<Option<WasmValue>, Trap> {
        check_bus(arg_i32(args, 0)?)?;
        let addr = arg_i32(args, 1)?;
        let mut s = s.borrow_mut();
        let method = s.probe;
        let rc = s.call_i32(method, vec![CilValue::Int32(addr as i32)])?;
        Ok(Some(WasmValue::I32(rc as u32)))
    };

    World {
        funcs: vec![
            HostFunc {
                module: String::from("lamella_i2c"),
                name: String::from("write"),
                ty: i2c_type(4),
                call: Box::new(write),
            },
            HostFunc {
                module: String::from("lamella_i2c"),
                name: String::from("read"),
                ty: i2c_type(4),
                call: Box::new(read),
            },
            HostFunc {
                module: String::from("lamella_i2c"),
                name: String::from("write_read"),
                ty: i2c_type(6),
                call: Box::new(write_read),
            },
            HostFunc {
                module: String::from("lamella_i2c"),
                name: String::from("probe"),
                ty: i2c_type(2),
                call: Box::new(probe),
            },
        ],
    }
}

/// The full eventful grant: the `lamella_i2c` data plane plus the `lamella_evt` event half,
/// both served by the SAME interpreted session -- the real-hardware counterpart of the world
/// [`lamella_wasm_interp::simulated_events::eventful_world`] provides without a device.
///
/// The readiness source is the accelerometer's own `ZYXDA` bit, reached through the
/// interpreted driver. That bit is a single latched flag which the guest's own burst read
/// clears, so the hardware's status register IS the depth-1 coalescing slot rather
/// than something the host has to simulate on top of it.
///
/// `now_ms` supplies the caller's millisecond clock, because a bounded wait is mandatory and
/// a bound needs a clock the host owns (the firmware passes its SysTick counter). `address`
/// is the part's bus address.
///
/// # Panics
/// If the session's harness carries no readiness half (`EnableDataReady` / `DataReady`) --
/// callers with an i2c-only harness want [`world`] instead.
#[must_use]
pub fn eventful_world(
    session: DriverSession,
    address: u32,
    now_ms: impl Fn() -> u64 + 'static,
) -> World {
    assert!(
        session.enable_data_ready.is_some() && session.data_ready.is_some(),
        "eventful_world needs a harness with the readiness half; use world() for i2c only"
    );
    let session = Rc::new(RefCell::new(session));
    let mut combined = world_over(&session);

    let s = Rc::clone(&session);
    let subscribe = move |_mem: &mut [u8], args: &[WasmValue]| -> Result<Option<WasmValue>, Trap> {
        if arg_i32(args, 0)? != EVT_SOURCE_DATA_READY {
            return Ok(Some(WasmValue::I32(EVT_NO_SUCH_SOURCE)));
        }
        let mut s = s.borrow_mut();
        let method = s.enable_data_ready.expect("readiness half bound");
        let rc = s.call_i32(method, vec![CilValue::Int32(address as i32)])?;
        if rc != 0 {
            return Ok(Some(WasmValue::I32(EVT_NO_SUCH_SOURCE)));
        }
        Ok(Some(WasmValue::I32(EVT_SUBSCRIPTION)))
    };

    let unsubscribe =
        move |_mem: &mut [u8], args: &[WasmValue]| -> Result<Option<WasmValue>, Trap> {
            let rc = i32::from(arg_i32(args, 0)? != EVT_SUBSCRIPTION);
            Ok(Some(WasmValue::I32(rc as u32)))
        };

    let s = Rc::clone(&session);
    let wait_ready = move |_mem: &mut [u8], args: &[WasmValue]| -> Result<Option<WasmValue>, Trap> {
        if arg_i32(args, 0)? != EVT_SUBSCRIPTION {
            return Ok(Some(WasmValue::I32(EVT_TIMED_OUT)));
        }
        let timeout_ms = u64::from(arg_i32(args, 1)?);
        let deadline = now_ms().saturating_add(timeout_ms);
        loop {
            let ready = {
                let mut s = s.borrow_mut();
                let method = s.data_ready.expect("readiness half bound");
                s.call_i32(method, vec![CilValue::Int32(address as i32)])?
            };
            if ready == 1 {
                return Ok(Some(WasmValue::I32(0)));
            }
            if now_ms() >= deadline {
                return Ok(Some(WasmValue::I32(EVT_TIMED_OUT)));
            }
        }
    };

    combined.funcs.extend([
        HostFunc {
            module: String::from("lamella_evt"),
            name: String::from("subscribe"),
            ty: i2c_type(2),
            call: Box::new(subscribe),
        },
        HostFunc {
            module: String::from("lamella_evt"),
            name: String::from("unsubscribe"),
            ty: i2c_type(1),
            call: Box::new(unsubscribe),
        },
        HostFunc {
            module: String::from("lamella_evt"),
            name: String::from("wait_ready"),
            ty: i2c_type(2),
            call: Box::new(wait_ready),
        },
    ]);
    combined
}
