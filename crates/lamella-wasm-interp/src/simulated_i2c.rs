//! A `lamella_i2c` world served by a SIMULATED device, so a guest can be run with no hardware
//! at all -- the no-hardware way in, and a worked example of what a host must supply to grant
//! the bus. It models an addressable I2C device the ordinary way: a register file behind a
//! sub-address pointer with the auto-increment convention, defaulting to an accelerometer that
//! answers the real part's WHO_AM_I. Against real hardware the SAME world is served instead by
//! the interpreter bridge to the C# driver, and the guest does not change.
//! **WHAT PASSING HERE DOES AND DOES NOT MEAN.** It models only behavior its datasheet states,
//! and the omissions are listed on [`RegisterDevice`] rather than left implicit. A simulator
//! agrees with whoever wrote it: it can show that bytes move correctly, and cannot show that
//! they MEAN what you think -- a guest green here can still be wrong against the part. Use it
//! to develop, then confirm on a device against something physics constrains (for an
//! accelerometer at rest, the magnitude of the three axes is 1 g whatever its orientation).

use crate::exec::{HostFunc, Trap, Value, World};
use crate::{FuncType, ValType};
use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;

/// Layer-1 `Ok`.
pub const OK: i32 = 0;
/// Layer-1 `AddressNack`: no device acknowledged the address phase -- data, not an error.
pub const ADDRESS_NACK: i32 = 1;
/// Layer-1 `DataNack`.
pub const DATA_NACK: i32 = 2;
/// Layer-1 `OtherError`.
pub const OTHER_ERROR: i32 = 3;

/// CTRL_REG1_A: ODR[7:4] selects power-down or the data rate (DS10265 Tables 33-35).
const CTRL_REG1_A: usize = 0x20;
/// STATUS_REG_A (DS10265 Tables 50-51).
const STATUS_REG_A: usize = 0x27;
/// OUT_X_L_A: first byte of the six-byte XYZ output frame.
const OUT_X_L_A: usize = 0x28;
/// STATUS_REG_A.ZYXDA -- "a new set of data is available" (Table 51).
const STATUS_ZYXDA: u8 = 0x08;

/// The simulated device: one I2C slave with a register file behind a sub-address pointer.
/// Register reads auto-increment; the SUB high bit (0x80, the ST convention) is accepted and
/// masked. Defaults to an LSM303AGR accelerometer: address 0x19, `WHO_AM_I_A` (0x0F) = 0x33.
///
/// The sampling half models EXACTLY the datasheet-stated behavior and nothing
/// more: the part powers up in power-down (`ODR[3:0] = 0000`, Table 34); while an ODR from
/// Table 35 is selected and the rig has scripted [`frames`](Self::frames), a sample set
/// LANDS each period under [`advance_micros`](Self::advance_micros) -- the frame is written
/// to OUT_X_L_A..OUT_Z_H_A, `STATUS_REG_A.ZYXDA` sets ("a new set of data is available",
/// Table 51), and [`landings`](Self::landings) counts the data-ready edge the event world
/// observes. Deliberately UNMODELED because DS10265's register description does not state
/// them: the ZYXDA clear condition (the bit latches; the slice's consumers depend only on
/// the set condition), the overrun bits (their trigger needs precisely those unstated
/// read-tracking semantics), the kHz ODR rows (1000/1001), axis-disable, and BDU's
/// split-pair hold (landings here are transaction-atomic by construction -- simulated time
/// only advances between bus transactions -- so the burst-read consumer's consistency
/// already holds).
pub struct RegisterDevice {
    /// The 7-bit device address that acknowledges.
    pub address: u32,
    /// The register file.
    pub regs: Vec<u8>,
    /// The current sub-address pointer (set by a write, advanced by reads).
    pub pointer: usize,
    /// The rig's scripted sample frames: each landing writes the next one, the last
    /// repeats. EMPTY means the simulated part produces no data even when enabled (a
    /// silent-sensor rig for timeout paths).
    pub frames: Vec<[u8; 6]>,
    /// Monotonic count of landed sample sets -- the data-ready edge fact `simulated_events` reads.
    pub landings: u64,
    /// Which scripted frame lands next.
    next_frame: usize,
    /// Simulated microseconds accumulated toward the next landing.
    accumulated_us: u32,
}

impl RegisterDevice {
    /// The LSM303AGR accelerometer's WHO_AM_I identity, per its datasheet facts.
    #[must_use]
    pub fn lsm303agr() -> RegisterDevice {
        let mut regs = vec![0u8; 0x30];
        regs[0x0F] = 0x33;
        RegisterDevice {
            address: 0x19,
            regs,
            pointer: 0,
            frames: Vec::new(),
            landings: 0,
            next_frame: 0,
            accumulated_us: 0,
        }
    }

    /// The selected sample period, from DS10265 Table 35: `0000` is power-down (no clock),
    /// `0001`..`0111` are 1/10/25/50/100/200/400 Hz. The kHz rows are unmodeled.
    fn sample_period_us(&self) -> Option<u32> {
        match self.regs.get(CTRL_REG1_A).copied().unwrap_or(0) >> 4 {
            0x1 => Some(1_000_000),
            0x2 => Some(100_000),
            0x3 => Some(40_000),
            0x4 => Some(20_000),
            0x5 => Some(10_000),
            0x6 => Some(5_000),
            0x7 => Some(2_500),
            _ => None,
        }
    }

    /// Advances simulated time, landing a sample set at each ODR period boundary. Time in
    /// power-down does not accumulate (the ODR clock is not running).
    pub fn advance_micros(&mut self, mut us: u32) {
        loop {
            let Some(period) = self.sample_period_us() else { return };
            if self.frames.is_empty() || self.regs.len() < OUT_X_L_A + 6 {
                return;
            }
            let to_landing = period - self.accumulated_us;
            if us < to_landing {
                self.accumulated_us += us;
                return;
            }
            us -= to_landing;
            self.accumulated_us = 0;
            let frame = self.frames[self.next_frame];
            if self.next_frame + 1 < self.frames.len() {
                self.next_frame += 1;
            }
            self.regs[OUT_X_L_A..OUT_X_L_A + 6].copy_from_slice(&frame);
            self.regs[STATUS_REG_A] |= STATUS_ZYXDA;
            self.landings += 1;
        }
    }

    fn write(&mut self, bytes: &[u8]) -> i32 {
        if let Some((sub, data)) = bytes.split_first() {
            self.pointer = usize::from(sub & 0x7F);
            for b in data {
                if self.pointer < self.regs.len() {
                    if self.pointer == CTRL_REG1_A {
                        self.accumulated_us = 0;
                    }
                    self.regs[self.pointer] = *b;
                }
                self.pointer += 1;
            }
        }
        OK
    }

    fn read(&mut self, out: &mut [u8]) -> i32 {
        for slot in out {
            *slot = self.regs.get(self.pointer).copied().unwrap_or(0);
            self.pointer += 1;
        }
        OK
    }
}

/// Bounds-checks a guest (ptr, len) pair against linear memory. A violation is a TRAP, never
/// a status: the module handed the host a buffer it does not have.
fn guest_range(mem: &[u8], ptr: u32, len: u32) -> Result<(usize, usize), Trap> {
    let start = ptr as u64;
    let end = start + u64::from(len);
    if end > mem.len() as u64 {
        return Err(Trap::MemOutOfBounds);
    }
    Ok((start as usize, end as usize))
}

pub(crate) fn arg_i32(args: &[Value], i: usize) -> Result<u32, Trap> {
    match args.get(i) {
        Some(Value::I32(v)) => Ok(*v),
        _ => Err(Trap::Host("simulated i2c: argument type")),
    }
}

/// A device shared between worlds: the `lamella_i2c` data plane and the `lamella_evt`
/// event half observe the SAME simulated part.
pub type SharedDevice = Rc<RefCell<RegisterDevice>>;

/// Wraps a device for sharing across world builders.
#[must_use]
pub fn shared(device: RegisterDevice) -> SharedDevice {
    Rc::new(RefCell::new(device))
}

/// The granted bus handle. The grant arrives pre-opened and pre-configured, so a guest never
/// opens or configures a bus: handle 0 is the board's bus, and nothing else exists.
const BUS: u32 = 0;

fn i2c_type(params: usize) -> FuncType {
    FuncType { params: vec![ValType::I32; params], results: vec![ValType::I32] }
}

/// Builds the granted `lamella_i2c` world around one simulated device: the four layer-1
/// projections (`write`, `read`, `write_read`, `probe`), sharing the device state.
#[must_use]
pub fn world(device: RegisterDevice) -> World {
    world_over(&shared(device))
}

/// [`world`], but over an externally shared device -- the shape `simulated_events` composes with.
#[must_use]
pub fn world_over(device: &SharedDevice) -> World {
    let device = Rc::clone(device);

    let status = |ok: bool| i32::from(!ok);

    let write_dev = Rc::clone(&device);
    let write = move |mem: &mut [u8], args: &[Value]| -> Result<Option<Value>, Trap> {
        let (bus, addr) = (arg_i32(args, 0)?, arg_i32(args, 1)?);
        let (ptr, len) = (arg_i32(args, 2)?, arg_i32(args, 3)?);
        if bus != BUS {
            return Err(Trap::Host("simulated i2c: unknown bus handle"));
        }
        let (start, end) = guest_range(mem, ptr, len)?;
        let mut dev = write_dev.borrow_mut();
        if addr != dev.address {
            return Ok(Some(Value::I32(ADDRESS_NACK as u32)));
        }
        let code = dev.write(&mem[start..end]);
        Ok(Some(Value::I32(code as u32)))
    };

    let read_dev = Rc::clone(&device);
    let read = move |mem: &mut [u8], args: &[Value]| -> Result<Option<Value>, Trap> {
        let (bus, addr) = (arg_i32(args, 0)?, arg_i32(args, 1)?);
        let (ptr, len) = (arg_i32(args, 2)?, arg_i32(args, 3)?);
        if bus != BUS {
            return Err(Trap::Host("simulated i2c: unknown bus handle"));
        }
        let (start, end) = guest_range(mem, ptr, len)?;
        let mut dev = read_dev.borrow_mut();
        if addr != dev.address {
            return Ok(Some(Value::I32(ADDRESS_NACK as u32)));
        }
        let code = dev.read(&mut mem[start..end]);
        Ok(Some(Value::I32(code as u32)))
    };

    let wr_dev = Rc::clone(&device);
    let write_read = move |mem: &mut [u8], args: &[Value]| -> Result<Option<Value>, Trap> {
        let (bus, addr) = (arg_i32(args, 0)?, arg_i32(args, 1)?);
        let (wptr, wlen) = (arg_i32(args, 2)?, arg_i32(args, 3)?);
        let (rptr, rlen) = (arg_i32(args, 4)?, arg_i32(args, 5)?);
        if bus != BUS {
            return Err(Trap::Host("simulated i2c: unknown bus handle"));
        }
        let (wstart, wend) = guest_range(mem, wptr, wlen)?;
        let (rstart, rend) = guest_range(mem, rptr, rlen)?;
        if rlen < 1 {
            return Ok(Some(Value::I32(OTHER_ERROR as u32)));
        }
        let mut dev = wr_dev.borrow_mut();
        if addr != dev.address {
            return Ok(Some(Value::I32(ADDRESS_NACK as u32)));
        }
        let sub: Vec<u8> = mem[wstart..wend].to_vec();
        let mut code = dev.write(&sub);
        if code == OK {
            let mut out = vec![0u8; rend - rstart];
            code = dev.read(&mut out);
            mem[rstart..rend].copy_from_slice(&out);
        }
        Ok(Some(Value::I32(code as u32)))
    };

    let probe_dev = Rc::clone(&device);
    let probe = move |_mem: &mut [u8], args: &[Value]| -> Result<Option<Value>, Trap> {
        let (bus, addr) = (arg_i32(args, 0)?, arg_i32(args, 1)?);
        if bus != BUS {
            return Err(Trap::Host("simulated i2c: unknown bus handle"));
        }
        let ok = addr == probe_dev.borrow().address;
        Ok(Some(Value::I32(status(ok) as u32)))
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
