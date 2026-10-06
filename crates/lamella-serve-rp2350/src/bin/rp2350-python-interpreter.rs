//! The Python firmware for the Raspberry Pi Pico 2 and Pico 2 W (RP2350, Cortex-M33): the Python
//! bytecode interpreter linked bare-metal, running a bundle the host compiled and printing what the
//! program prints over the board's UART0 console.
//!
//! On the Pico 2 W, Python's `socket` reaches the radio: the `python-net` build hands the
//! interpreter a `smoltcp` stack over the CYW43439's ethernet data channel -- the same
//! `lamella-net-smoltcp` mapping of the same `lamella-net-core` seam that carries C# evaluations on
//! this board and Python on the SAM E54's wired GMAC. Three consumers of one seam, so what a socket
//! does cannot depend on which language asked or which medium carried it.
//!
//! This board has a monotonic source and no date, and it says so. `set_monotonic` installs the
//! elapsed-time half alone; nothing anchors the wall clock, so `lamella.clock.is_set()` answers
//! False and `time.time()` reads as the epoch.
//!
//! The rate is read off the clock tree rather than declared, because the boot `clk_sys` is a range
//! of states and not a value. This firmware never programs the clock tree, so it runs at whatever
//! rate it was started into, and that differs by boot path: a board that came up through the ROM's
//! USB path inherits a PLL running at 48 MHz, while one whose `CLK_REF_CTRL` and `CLK_SYS_CTRL` are
//! still at their `0x00000000` reset values is taking `clk_ref` straight from the ring oscillator
//! at about a quarter of that. Both are ordinary states to boot into.
//!
//! So a constant naming any one boot rate is right on one path and wrong on the other, and nothing
//! on the board can tell which -- a self-timed program and the clock it is timed against scale
//! together, so they agree with each other at every rate. A tree whose rate is not derivable
//! therefore installs no clock rather than a plausible one, and `time.monotonic()` refuses on a
//! ring-oscillator boot instead of returning durations that are four times wrong. The refusal is
//! the feature. `rp2350-csharp-interpreter` takes the other route and programs the tree itself
//! (`rp2350_usb::clocks_init`), which is what makes its rate knowable rather than inherited.
//!
//! A native capability is named in every recipe that means to have it. This crate takes
//! `lamella-py-runtime` with `default-features = false`, so a recipe that omits `python-float`
//! builds a no-float image where `1 / 3` refuses -- a legitimate tier, never one to arrive at by
//! leaving a word out. `python-complex`, `python-reflection` and `python-threading` are the same
//! kind of word: native code that no host-compiled bundle can substitute, so omitting one takes
//! `1j`, `dir()` or `import threading` out of the language this board runs. A bundled module is not
//! like this, which is why `python-stdlib` is deliberately absent from every recipe here.
//!
//! `python-complex` is the one that cannot appear in the no-float recipe: a complex is a pair of
//! f64, so the feature pulls `python-float` in and would quietly turn that tier back into a float
//! build. It is omitted there for that reason and for no other.
//!
//! Board facts: Raspberry Pi Pico 2 W datasheet + the generated `bsp/rpi-pico2-w` bindings; chip
//! registers: RP2350 datasheet. The wireless -- the driver on its PIO engine, and the station that
//! makes it one network device -- is brought up by `pico_wifi.rs`, shared with the C# interpreter
//! firmware.
//!
//! Build: cargo build -p lamella-serve-rp2350 --bin rp2350-python-interpreter --profile flash
//!        --target thumbv8m.main-none-eabi --features python,rp2350,python-float,python-threading,python-complex,python-reflection
//! Build: cargo build -p lamella-serve-rp2350 --bin rp2350-python-interpreter --profile flash
//!        --target thumbv8m.main-none-eabi --features python-net,rp2350,python-float,python-threading,python-complex,python-reflection (hygiene-allow: a cargo feature, not a crate)
//! Build: cargo build -p lamella-serve-rp2350 --bin rp2350-python-interpreter --profile flash
//!        --target thumbv8m.main-none-eabi --features python-net,rp2350,python-threading,python-reflection (the no-float tier, on purpose)
//! Flash: objcopy to .bin and program at 0x10000000 over the debug probe, or drag a UF2 to BOOTSEL.
//!
//! The recipes above carry no credentials, and a `python-net` image built without them is a real
//! build rather than a broken one: `LAMELLA_WIFI_SSID`, `LAMELLA_WIFI_PSK` and
//! `LAMELLA_WIFI_SECURITY` (`wpa3` for a WPA3 network; WPA2 otherwise) are `option_env!` reads in
//! the shared `pico_wifi.rs`, so no network's secret is ever in this tree, and absent them the
//! bring-up stops after radio-up and says so. Such a build proves the image links and nothing
//! about whether it associates. Set them to run it against a network, and set
//! `LAMELLA_PY_BUNDLE` to the program it should run. A `python-net` build also downloads the radio's
//! images and checks them against their pinned hashes; with no network, point
//! `LAMELLA_WIFI_IMAGES_DIR` at a folder fetched earlier.
#![cfg_attr(target_os = "none", no_std)]
#![cfg_attr(target_os = "none", no_main)]

/// The PICOBIN IMAGE_DEF block the RP2350 bootrom validates: a single self-looping block marking an
/// Arm RP2350 EXE (datasheet 5.9.5.1). Without it the bootrom finds no image and never runs this.
#[cfg(target_os = "none")]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".embedded_block")]
#[used]
static IMAGE_DEF: [u32; 5] = [0xffff_ded3, 0x1021_0142, 0x0000_01ff, 0x0000_0000, 0xab12_3579];

#[cfg(target_os = "none")]
#[path = "../../../lamella-serve-core/src/startup.rs"]
mod startup;

#[cfg(all(target_os = "none", not(feature = "python-net")))]
#[path = "../../../../bsp/rpi-pico2/rust/rpi_pico2_bindings.rs"]
#[allow(dead_code)]
mod board_bindings;
#[cfg(all(target_os = "none", feature = "python-net"))]
#[path = "../../../../bsp/rpi-pico2-w/rust/rpi_pico2_w_bindings.rs"]
#[allow(dead_code)]
mod board_bindings;
#[cfg(target_os = "none")]
#[path = "../../../../csp/rp2350/rust/rp2350_instances.rs"]
#[allow(dead_code)]
mod rp2350_instances;

#[cfg(target_os = "none")]
#[allow(dead_code)]
#[path = "../rp2350_uart.rs"]
mod rp2350_uart;

#[cfg(target_os = "none")]
#[path = "../../../lamella-serve-core/src/systick_clock.rs"]
mod systick_clock;

#[cfg(target_os = "none")]
#[allow(dead_code)]
#[path = "../../../lamella-serve-core/src/rp2350_clocks.rs"]
mod rp2350_clocks;

#[cfg(all(target_os = "none", feature = "python-net"))]
#[allow(dead_code)]
#[path = "../pico_wifi.rs"]
mod pico_wifi;

#[cfg(all(target_os = "none", feature = "python-net"))]
#[path = "../../../lamella-serve-core/src/rp2350_trng.rs"]
mod rp2350_trng;

#[cfg(target_os = "none")]
mod python {
    extern crate alloc;
    use alloc::vec::Vec;
    use core::sync::atomic::{AtomicUsize, Ordering};
    use lamella_py_runtime::{Bundle, ObjectModel, Trap, run_bundle};

    use crate::board_bindings as board;
    use crate::rp2350_clocks;
    use crate::rp2350_instances as chip;
    use crate::rp2350_uart as uart;

    const ARENA_BYTES: usize = 256 * 1024;
    #[allow(dead_code)]
    #[repr(align(16))]
    struct Arena([u8; ARENA_BYTES]);
    static mut ARENA: Arena = Arena([0; ARENA_BYTES]);

    include!(concat!(env!("OUT_DIR"), "/py_heap.rs"));

    /// Bytes currently handed out, maintained here because the heap does not track it and the
    /// collector's trigger needs it.
    ///
    /// `LockedHeap` can answer two other questions -- the carved high-water, lock-free, and the
    /// free-list total, which takes the lock and walks every size class -- and neither is this one.
    /// The high-water never falls, so a trigger reading it would find itself over threshold forever
    /// however much a collection reclaimed; the free-list walk is `O(free blocks)` and this is read
    /// at every safe point, before every op.
    static ARENA_LIVE: AtomicUsize = AtomicUsize::new(0);

    /// The size of the allocation that failed, recorded lock-free at the failure site so the
    /// allocation-free panic handler can print it: "how big was the request" is the one number that
    /// separates a genuine budget miss from class fragmentation.
    static LAST_FAILED_ALLOC: AtomicUsize = AtomicUsize::new(0);

    struct CountedHeap(lamella_heap::LockedHeap);

    unsafe impl alloc::alloc::GlobalAlloc for CountedHeap {
        unsafe fn alloc(&self, layout: alloc::alloc::Layout) -> *mut u8 {
            let pointer = unsafe { self.0.alloc(layout) };
            if pointer.is_null() {
                LAST_FAILED_ALLOC.store(layout.size(), Ordering::Relaxed);
            } else {
                ARENA_LIVE.fetch_add(layout.size(), Ordering::Relaxed);
            }
            pointer
        }

        unsafe fn dealloc(&self, pointer: *mut u8, layout: alloc::alloc::Layout) {
            ARENA_LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
            unsafe { self.0.dealloc(pointer, layout) };
        }
    }

    #[global_allocator]
    static ALLOCATOR: CountedHeap = CountedHeap(lamella_heap::LockedHeap::empty());

    /// The arena high-water: how much was ever carved. The right figure for a footprint report, and
    /// monotonic by construction. Lock-free, so the panic path never risks the heap lock.
    fn arena_used() -> usize {
        ALLOCATOR.0.carved_lockfree()
    }

    /// What is in use now -- a different question from [`arena_used`], and the one a collector has to
    /// ask. A figure that never falls says a collection reclaimed nothing however much it reclaimed,
    /// so a trigger reading the high-water would keep raising its own bar until the arena ran out.
    fn arena_live() -> usize {
        ARENA_LIVE.load(Ordering::Relaxed)
    }

    /// What this part's one allocator has handed out, and how much there is: the answer
    /// `ObjectModel::set_arena_probe` asks for, so a collection is driven by what is actually running
    /// out.
    ///
    /// The object heap is the wrong quantity to judge pressure by here, and it is the smaller one.
    /// It is a single block taken from this arena at startup; a program's decoded bundle, its strings
    /// and its side tables all come from the arena around it, so the heap can sit comfortably below
    /// its threshold while the arena that has to hold everything fills. On a `python-net` build the
    /// wireless stack's buffers are on that same arena too.
    ///
    /// Constant time, which is a requirement rather than a nicety: this is read at every safe
    /// point, before every op.
    #[cfg(feature = "python-gc-engine")]
    fn arena_probe() -> (usize, usize) {
        (arena_live(), ARENA_BYTES)
    }

    #[panic_handler]
    fn panic(info: &core::panic::PanicInfo) -> ! {
        uart::str("\r\n[py] firmware abort ");
        if let Some(location) = info.location() {
            uart::str(location.file());
            uart::str(":");
            uart::decimal(location.line() as usize);
        }
        if let Some(message) = info.message().as_str() {
            uart::str(" ");
            uart::str(message);
        }
        uart::str(" arena ");
        uart::decimal(arena_used());
        uart::str(" of ");
        uart::decimal(ARENA_BYTES);
        let failed = LAST_FAILED_ALLOC.load(Ordering::Relaxed);
        if failed != 0 {
            uart::str(" failed-alloc=");
            uart::decimal(failed);
        }
        uart::str("\r\n");
        loop {}
    }

    fn write_register(address: usize, value: u32) {
        unsafe { core::ptr::write_volatile(address as *mut u32, value) };
    }

    /// The part's own registers, for the TRNG driver.
    #[cfg(feature = "python-net")]
    struct Part;

    #[cfg(feature = "python-net")]
    impl crate::rp2350_trng::Registers for Part {
        fn read(&mut self, address: usize) -> u32 {
            unsafe { core::ptr::read_volatile(address as *const u32) }
        }
        fn write(&mut self, address: usize, value: u32) {
            write_register(address, value);
        }
    }

    /// Point the vector table at the flash base so faults reach [`fault`] (the bootrom does not
    /// guarantee VTOR).
    const SCB_VTOR: usize = 0xe000_ed08;

    /// The interpreter's monotonic seam over the shared `systick_clock`.
    ///
    /// The resolution is a millisecond, not a nanosecond, and the unit belongs to the seam rather
    /// than being a claim about the counter. A program timing something shorter than a millisecond on
    /// this board measures zero, which is the honest answer from a clock read this way.
    mod board_clock {
        use crate::systick_clock;

        /// Nanoseconds since the clock was installed.
        pub fn now_ns() -> i64 {
            (systick_clock::now_ms() as i64).saturating_mul(1_000_000)
        }

        /// The interpreter's sleep seam. Rounds a sub-millisecond request UP to one millisecond
        /// rather than to zero: a `sleep` that returns at once is the one answer a caller asking to
        /// wait ruled out, and it is the same rounding rule the socket timeout seam uses for the
        /// same reason.
        pub fn sleep_ns(nanos: i64) {
            if nanos <= 0 {
                return;
            }
            let millis = ((nanos + 999_999) / 1_000_000) as u64;
            systick_clock::sleep_ms(millis);
        }
    }

    /// Brings the wireless up and hands the interpreter a backend, then states what it got.
    ///
    /// The lines it prints are the whole diagnosis when a socket program does nothing: a board that
    /// never joined, a board that joined and got no lease, and a board with a dead peer look
    /// identical from the program's side, and only one of them is the program's fault.
    #[cfg(feature = "python-net")]
    fn install_net(model: &mut ObjectModel, ticks_per_ms: u32) {
        use crate::pico_wifi;
        use lamella_net_core::NetBackend;

        let before = arena_live();
        let settings = alloc::boxed::Box::new(lamella_wifi_cyw4343x_smoltcp::record::NoStore);
        pico_wifi::ensure_wifi(ticks_per_ms.saturating_mul(1000), settings, &mut |line: &str| {
            uart::str(line);
            uart::str("\r\n");
        });
        if !pico_wifi::wifi_up() {
            uart::str("[net] wifi is NOT up; the program runs with no backend\r\n");
            return;
        }
        let device = pico_wifi::device();
        let mut config =
            lamella_net_smoltcp::NetConfig::new(pico_wifi::mac(), lamella_net_smoltcp::IpSetup::Dhcp);
        config.tcp_rx_buffer = 2048;
        config.tcp_tx_buffer = 2048;
        config.listen_backlog = 1;
        config.would_block_grace_ms = 0;
        let mut seed_bytes = [0u8; 8];
        let from_trng = crate::rp2350_trng::start(
            &mut Part,
            chip::RESETS_BASE as usize,
            chip::RESETS_CLR_BASE as usize,
            chip::TRNG_RESET_MASK,
            chip::TRNG_BASE as usize,
        ) && crate::rp2350_trng::fill(&mut Part, chip::TRNG_BASE as usize, &mut seed_bytes, &mut || {
            crate::systick_clock::now_ms()
        });
        let seed = if from_trng {
            u64::from_le_bytes(seed_bytes)
        } else {
            uart::str("[net] the TRNG gave no entropy, so the network seed is the MAC and the uptime\r\n");
            let mac = pico_wifi::mac();
            u64::from(u32::from_le_bytes([mac[2], mac[3], mac[4], mac[5]])) ^ crate::systick_clock::now_ms()
        };
        let mut net = lamella_net_smoltcp::SmoltcpNet::new(
            device,
            config,
            crate::systick_clock::now_ms,
            seed,
        );
        let deadline = crate::systick_clock::now_ms().saturating_add(8_000);
        while net.ipv4_addr().is_none() && crate::systick_clock::now_ms() < deadline {
            let _ = net.poll(Some(100));
        }
        match net.ipv4_addr() {
            None => uart::str("[net] joined, no address YET (dhcp still trying)\r\n"),
            Some(ip) => {
                uart::str("[net] joined, ip ");
                uart::decimal(ip[0] as usize);
                uart::str(".");
                uart::decimal(ip[1] as usize);
                uart::str(".");
                uart::decimal(ip[2] as usize);
                uart::str(".");
                uart::decimal(ip[3] as usize);
                uart::str("\r\n");
            }
        }
        uart::str("[net] stack at bring-up ");
        uart::decimal(arena_live().saturating_sub(before));
        uart::str(" bytes, before any socket | arena ");
        uart::decimal(arena_live());
        uart::str(" live of ");
        uart::decimal(ARENA_BYTES);
        uart::str("\r\n");
        model.set_net_backend(alloc::boxed::Box::new(net));
    }

    /// What the run cost in RAM, measured rather than modeled, and split -- because the split is the
    /// part that decides how long a program lasts.
    ///
    /// The object heap is one allocation out of the arena, reserved whole at startup whether a
    /// program fills it or not. Everything the model holds beside it -- interned strings, byte
    /// buffers, the sequence, dict and set arenas, module namespaces, call frames -- and the network
    /// stack's own buffers are separate allocations on the same arena, and are not counted against
    /// the heap's capacity. So the heap figure alone understates what a program costs.
    ///
    /// The live and high-water figures are both reported because they answer different questions:
    /// the high-water is what the program ever needed, and the live figure is what a collection left
    /// behind.
    fn report_arena(model: &ObjectModel) {
        uart::str("[py] arena ");
        uart::decimal(arena_used());
        uart::str(" high-water, ");
        uart::decimal(arena_live());
        uart::str(" live, of ");
        uart::decimal(ARENA_BYTES);
        uart::str(" | object heap ");
        uart::decimal(model.heap().used() as usize);
        uart::str(" of ");
        uart::decimal(PY_HEAP_BYTES);
        uart::str(" reserved\r\n");
    }

    /// The bundle this firmware runs, placed by the build script: the file `LAMELLA_PY_BUNDLE` names,
    /// or an empty placeholder when it names none. The placeholder arm is what measures the
    /// interpreter's flash cost on its own.
    static BUNDLE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/bundle.lpyc"));

    /// Any exception/fault vector lands here.
    #[unsafe(no_mangle)]
    pub extern "C" fn fault() -> ! {
        uart::str("\r\n[py] fault\r\n");
        loop {}
    }

    /// The name a trap reports on the wire. An uncaught Python exception carries its own type name
    /// from the model; these are the interpreter's own failure reasons, which a Python program cannot
    /// catch and which a reader needs told apart from the program's output.
    fn trap_name(trap: Trap) -> &'static str {
        match trap {
            Trap::OutOfMemory => "OutOfMemory",
            Trap::Overflow => "Overflow",
            Trap::RecursionError => "RecursionError",
            Trap::Unsupported => "Unsupported",
            Trap::Malformed => "Malformed",
            // Every live thread is waiting for another one. It reaches the wire by name because a
            // board that stops is otherwise indistinguishable from a board that finished, and the
            // scheduler has already said which thread is waiting for which on stream 1.
            Trap::Deadlock => "Deadlock",
            _ => "Exception",
        }
    }

    /// Installs the monotonic clock if the tree can say what rate it runs at, and answers the tick
    /// scale -- `clk_sys` in kilohertz, which the wireless's PIO engine also needs.
    ///
    /// No fallback number, deliberately. `clk_sys` may be on the ROSC (which RP2350 A3
    /// randomizes, bounded only to 18.4-96.0 MHz) or on a GPIO input, so substituting a plausible
    /// constant makes every duration wrong by whatever the ratio happens to be, while no on-board
    /// test can see it -- a board timing itself divides by the same wrong number twice and agrees
    /// with itself. Installing nothing leaves `time.monotonic()` refusing, which is the honest
    /// state.
    fn install_clock() -> Option<u32> {
        let tree = rp2350_clocks::read_tree(
            chip::CLOCKS_BASE as usize,
            chip::PLL_SYS_BASE as usize,
            chip::PLL_USB_BASE as usize,
        );
        match rp2350_clocks::clk_sys_hz_from(&tree, board::XOSC_HZ_PLL_150_48) {
            Some(clk_sys_hz) => {
                crate::systick_clock::install(clk_sys_hz);
                Some(clk_sys_hz / 1000)
            }
            None => {
                uart::str("[py] no clock: clk_sys runs from ");
                uart::str(rp2350_clocks::clk_sys_source_name(&tree));
                uart::str(", whose rate is not derivable\r\n");
                None
            }
        }
    }

    /// The post-startup entry (`startup::reset` has zeroed .bss and copied .data).
    #[unsafe(no_mangle)]
    pub extern "C" fn lamella_main() -> ! {
        write_register(SCB_VTOR, 0x1000_0000);
        unsafe {
            ALLOCATOR.0.init(core::ptr::addr_of_mut!(ARENA).cast::<u8>(), ARENA_BYTES);
        }
        uart::init();
        uart::str("[py] boot\r\n");
        let ticks_per_ms = install_clock();

        let image: &'static [u8] = core::hint::black_box(BUNDLE);

        match Bundle::decode(image) {
            Ok((bundle, _features)) => {
                let mut model = ObjectModel::new(Vec::new(), PY_HEAP_BYTES);
                model.set_console(uart::str);
                if ticks_per_ms.is_some() {
                    model.set_monotonic(board_clock::now_ns, board_clock::sleep_ns);
                }
                #[cfg(feature = "python-gc-engine")]
                model.set_arena_probe(arena_probe);
                #[cfg(feature = "python-gc-engine")]
                model.set_collect_when_full(true);
                #[cfg(feature = "python-net")]
                match ticks_per_ms {
                    Some(ticks) => install_net(&mut model, ticks),
                    None => uart::str(
                        "[net] no derivable clock, so the radio is not brought up: its bus rate and \
                         its deadlines are both set from clk_sys\r\n",
                    ),
                }
                let outcome = run_bundle(bundle, &mut model);
                uart::str(&model.take_stdout());
                match outcome {
                    Ok(_) => uart::str("\r\n[py] done\r\n"),
                    Err(trap) => {
                        uart::str("\r\n[py] trap=");
                        uart::str(trap_name(trap));
                        uart::str("\r\n");
                    }
                }
                report_arena(&model);
            }
            Err(_) => uart::str("\r\n[py] no bundle\r\n"),
        }

        loop {}
    }
}

#[cfg(not(target_os = "none"))]
fn main() {
    println!(
        "rp2350-python-interpreter is a bare-metal target; build for thumbv8m.main-none-eabi with the python feature"
    );
}
