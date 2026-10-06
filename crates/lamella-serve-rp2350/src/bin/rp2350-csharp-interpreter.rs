//! The C# interpreter firmware for RP2350 boards (the Raspberry Pi Pico 2 and Pico 2 W, and the
//! Pimoroni Pico Plus 2 and Plus 2 W; Cortex-M33 / ARMv8-M): the on-device half of a Lamella Link
//! deploy target, taking programs from the lamella CLI. Speaks lamella-wire over UART0 (PL011, TX
//! GP0 / RX GP1, 115200 8N1, bridged by the Raspberry Pi Debug Probe) and -- with the `usb` feature
//! -- over the chip's own USB port as a driverless-WinUSB device (rp2350_usb.rs: a hand-rolled
//! RP2350 device controller under the shared usb_transport carrier; VID/PID shared with the Lamella
//! Link family, the product string naming the board and the OTP chip id as the unique serial).
//! HELLO / LOAD_* / EXEC / DEPLOY_* work over either carrier, replies following the requester.
//!
//! A chunked DEPLOY_IMAGE streams a baked image into flash at 0x10200000 through the bootrom flash
//! API (rp2350_flash.rs); on reset the boot path runs a stored image on the interpreter, staying
//! interruptible by a host HELLO between step bursts. A panic or fault stamps a retained flag and
//! resets into the command loop, so a crashing app cannot brick the board into a crash loop. Both
//! carriers are polled -- the PL011's 32-byte RX FIFO covers the command and debug loops' poll
//! cadence at 115200, and the USB device controller is serviced by the same polls -- so the vector
//! table carries one interrupt only: IO_IRQ_BANK0, whose handler queues the pin-change events a
//! program's GPIO callbacks hear (rp2350_pin_events.rs).
//!
//! Boot: the RP2350 bootrom scans the first 4 KB for the PICOBIN IMAGE_DEF block (below) and
//! boots the vector table at 0x10000000. Peripheral facts: RP2350 datasheet -- XOSC 12 MHz ->
//! clk_peri; UART0 = PL011 @ 0x40070000, TX GP0 / RX GP1 (funcsel 2); the USB build raises
//! clk_sys to 150 MHz + clk_usb to 48 MHz (rp2350_usb::clocks_init).
//! Build: cargo build -p lamella-serve-rp2350 --bin rp2350-csharp-interpreter --profile flash-speed
//!        --target thumbv8m.main-none-eabi --features serve,rp2350
//! Build: cargo build -p lamella-serve-rp2350 --bin rp2350-csharp-interpreter --profile flash-speed
//!        --target thumbv8m.main-none-eabi --features serve,rp2350,usb,resident-corlib
//! Build: cargo build -p lamella-serve-rp2350 --bin rp2350-csharp-interpreter --profile flash-speed
//!        --target thumbv8m.main-none-eabi --features serve,rp2350,usb,cyw43
//! Build: cargo build -p lamella-serve-rp2350 --bin rp2350-csharp-interpreter --profile flash-speed
//!        --target thumbv8m.main-none-eabi --features serve,rp2350,usb,pico-plus-2-w
//! Build: cargo build -p lamella-serve-rp2350 --bin rp2350-csharp-interpreter --profile flash-speed
//!        --target thumbv8m.main-none-eabi --features serve,rp2350,usb,pico-plus-2
//! Build: cargo build -p lamella-serve-rp2350 --bin rp2350-csharp-interpreter --profile flash-speed
//!        --target thumbv8m.main-none-eabi --features serve,rp2350,usb,cyw43,tls,system-roots
//! Build: cargo build -p lamella-serve-rp2350 --bin rp2350-csharp-interpreter --profile flash-speed
//!        --target thumbv8m.main-none-eabi --features serve,rp2350,usb,pico-plus-2-w,tls,system-roots
//! Build: cargo build -p lamella-serve-rp2350 --bin rp2350-csharp-interpreter --profile flash-speed
//!        --target thumbv8m.main-none-eabihf --features serve,rp2350
//! Build: cargo build -p lamella-serve-rp2350 --bin rp2350-csharp-interpreter --profile flash-speed
//!        --target thumbv8m.main-none-eabihf --features serve,rp2350,usb,cyw43,tls,system-roots
//! Build: cargo build -p lamella-serve-rp2350 --bin rp2350-csharp-interpreter --profile flash-speed
//!        --target thumbv8m.main-none-eabihf --features serve,rp2350,usb,pico-plus-2-w,tls,system-roots
//! The fourth is the Pimoroni Pico Plus 2 W: the third's radio and serve, with that board's own
//! board file and identity. The fifth is the Pimoroni Pico Plus 2: the Pico 2's firmware, with no
//! radio, and likewise that board's own board file and identity. Each build's deploy window runs
//! from 0x10200000 to the end of the flash its board's facts state: 4 MB on the Pico 2 and Pico 2 W,
//! 16 MB on both Plus boards.
//! The sixth and seventh are the third and fourth with TLS: `tls` puts the vendored mbedTLS behind
//! the runtime's TLS seam, and `system-roots` adds the bundled CA store. They compile mbedTLS and so
//! need an ARM cross C compiler: `LAMELLA_ARM_GCC`, or `arm-none-eabi-gcc` on PATH (see
//! `lamella-tls-mbedtls`'s README).
//! The last three are hard-float builds of the first and of the two TLS builds: the same firmware for
//! `thumbv8m.main-none-eabihf`, where single-precision arithmetic runs on the M33's FPU. Its FPU has
//! no double-precision instructions, so `double` stays in software on both targets.
//! The third is the Pico 2 W, and it is listed for the reason the second is: a build gate covers the
//! feature combinations it is GIVEN, and this one selects a whole board file, a driver crate and a
//! network stack that the other two compile nothing of.
//! The second is the DEPLOY tier this board is demonstrated on -- the native-USB carrier alongside
//! the UART, and a flash-resident corlib so a deployed artifact can be the program's PE alone. It
//! is listed as its own recipe because a build gate only covers the feature combinations it is
//! given, and this one selects `#[cfg]` arms the base build compiles nothing of.
//! Flash: objcopy to .bin; program at 0x10000000 over a debug probe.
#![cfg_attr(target_os = "none", no_std)]
#![cfg_attr(target_os = "none", no_main)]

/// The PICOBIN IMAGE_DEF block the RP2350 bootrom validates: a single self-looping block
/// marking an Arm RP2350 EXE (datasheet 5.9.5.1).
#[cfg(target_os = "none")]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".embedded_block")]
#[used]
static IMAGE_DEF: [u32; 5] = [0xffff_ded3, 0x1021_0142, 0x0000_01ff, 0x0000_0000, 0xab12_3579];

#[cfg(target_os = "none")]
#[path = "../../../lamella-serve-core/src/startup.rs"]
mod startup;

#[cfg(target_os = "none")]
#[path = "../rp2350_flash.rs"]
mod rp2350_flash;

#[cfg(all(target_os = "none", not(feature = "cyw43"), not(feature = "pico-plus-2")))]
#[path = "../../../../bsp/rpi-pico2/rust/rpi_pico2_bindings.rs"]
#[allow(dead_code)]
mod board_bindings;
#[cfg(all(target_os = "none", feature = "pico-plus-2"))]
#[path = "../../../../bsp/pimoroni-pico-plus-2/rust/pimoroni_pico_plus_2_bindings.rs"]
#[allow(dead_code)]
mod board_bindings;
#[cfg(all(target_os = "none", feature = "cyw43", not(feature = "pico-plus-2-w")))]
#[path = "../../../../bsp/rpi-pico2-w/rust/rpi_pico2_w_bindings.rs"]
#[allow(dead_code)]
mod board_bindings;
#[cfg(all(target_os = "none", feature = "pico-plus-2-w"))]
#[path = "../../../../bsp/pimoroni-pico-plus-2-w/rust/pimoroni_pico_plus_2_w_bindings.rs"]
#[allow(dead_code)]
mod board_bindings;
#[cfg(target_os = "none")]
#[path = "../../../../csp/rp2350/rust/rp2350_instances.rs"]
#[allow(dead_code)]
mod rp2350_instances;

#[cfg(target_os = "none")]
#[path = "../../../lamella-serve-core/src/bare_critical_section.rs"]
mod bare_critical_section;

#[cfg(target_os = "none")]
#[path = "../../../lamella-serve-core/src/pin_events.rs"]
mod pin_events;

#[cfg(target_os = "none")]
#[path = "../rp2350_pin_events.rs"]
mod rp2350_pin_events;

#[cfg(all(target_os = "none", feature = "usb"))]
#[path = "../../../lamella-serve-core/src/usb_transport.rs"]
mod usb_transport;

#[cfg(target_os = "none")]
#[path = "../../../lamella-serve-core/src/systick_clock.rs"]
mod systick_clock;

#[cfg(target_os = "none")]
#[path = "../../../lamella-serve-core/src/rp2350_clocks.rs"]
mod rp2350_clocks;

#[cfg(all(target_os = "none", feature = "usb"))]
#[path = "../rp2350_usb.rs"]
mod rp2350_usb;

#[cfg(all(target_os = "none", feature = "cyw43"))]
#[path = "../pico_wifi.rs"]
mod pico_wifi;

#[cfg(all(target_os = "none", feature = "cyw43"))]
#[path = "../../../lamella-serve-core/src/rp2350_trng.rs"]
mod rp2350_trng;

#[cfg(all(target_os = "none", feature = "tls"))]
#[path = "../../../lamella-serve-core/src/tls_clock.rs"]
mod tls_clock;

#[cfg(target_os = "none")]
#[path = "../../../../csp/rp2350/rust/rp2350_xosc_layout.rs"]
#[allow(dead_code)]
mod rp2350_xosc_layout;

#[cfg(target_os = "none")]
mod serve {
    extern crate alloc;
    use crate::rp2350_flash::Rp2350Flash;
    use lamella_runner::carriers::{Carrier, CarrierSet, Windows};
    use lamella_runner::{Served, run_deployed_with, serve_one_deploy_with_residence};
    use lamella_wire::{Frame, FrameReader, Transport, TransportError, encode_frame};
    #[cfg(feature = "usb")]
    use crate::rp2350_usb;
    #[cfg(feature = "usb")]
    use crate::usb_transport::{self, UsbCarrier};

    unsafe extern "C" {
        /// The first byte past the statics (`memory-rp2350.x`), where the heap starts.
        static mut _sheap: u8;
        /// The stack's floor (`memory-rp2350.x`): where the heap ends, and the lowest address the
        /// stack may reach.
        static _stack_floor: u8;
    }

    /// The heap: every byte of RAM between the statics and the stack's floor, as the link laid it
    /// out. The firmware and the program share it. A reclaiming heap (lamella-heap, the O(1)
    /// segregated allocator), so dropped interpreter allocations return to it and a deployed
    /// infinite blink loop runs unbounded where a grow-only bump allocator would exhaust. The M33
    /// has the CAS the lock needs.
    fn heap() -> (*mut u8, usize) {
        let start = core::ptr::addr_of_mut!(_sheap);
        let end = core::ptr::addr_of!(_stack_floor) as usize;
        (start, end - start as usize)
    }

    /// Hands the core the stack's floor (ARMv8-M `MSPLIM`), so a stack that outgrows its share
    /// faults instead of writing into the heap below it.
    fn guard_stack() {
        let floor = core::ptr::addr_of!(_stack_floor) as u32;
        unsafe { core::arch::asm!("msr MSPLIM, {0}", in(reg) floor, options(nomem, nostack, preserves_flags)) };
    }

    #[global_allocator]
    static ALLOCATOR: lamella_heap::LockedHeap = lamella_heap::LockedHeap::empty();

    /// A firmware-level abort (an OOM, a HardFault) must not brick a deployed board: stamp the
    /// recovery flag and reset, so the next boot waits for the host rather than re-running the app that
    /// aborted. (A safe interpreted app's own trap does NOT reach here -- run_deployed returns
    /// it cleanly.)
    #[panic_handler]
    fn panic(info: &core::panic::PanicInfo) -> ! {
        let _ = core::fmt::Write::write_fmt(
            &mut UartWriter,
            format_args!(
                "[lamella] FIRMWARE ABORT: {info}\r\n[lamella] heap: {} of {} bytes carved, {} live\r\n",
                ALLOCATOR.carved_lockfree(),
                heap().1,
                ALLOCATOR.live_lockfree()
            ),
        );
        reset_to_serve()
    }

    fn write_register(address: usize, value: u32) {
        unsafe { core::ptr::write_volatile(address as *mut u32, value) };
    }

    fn read_register(address: usize) -> u32 {
        unsafe { core::ptr::read_volatile(address as *const u32) }
    }

    /// The part's own registers, for the TRNG driver.
    #[cfg(feature = "cyw43")]
    struct Part;

    #[cfg(feature = "cyw43")]
    impl crate::rp2350_trng::Registers for Part {
        fn read(&mut self, address: usize) -> u32 {
            read_register(address)
        }
        fn write(&mut self, address: usize, value: u32) {
            write_register(address, value);
        }
    }

    /// Takes the TRNG out of reset and configures it. Calling it again is harmless, so every
    /// evaluation does.
    #[cfg(feature = "cyw43")]
    fn trng_start() -> bool {
        crate::rp2350_trng::start(
            &mut Part,
            chip::RESETS_BASE as usize,
            chip::RESETS_CLR_BASE as usize,
            chip::TRNG_RESET_MASK,
            chip::TRNG_BASE as usize,
        )
    }

    /// Fills `buffer` from the TRNG: the network stack's seed, and the TLS engine's entropy source.
    /// `false` when it could not, so a TLS configuration fails loudly rather than run on weak keys.
    #[cfg(feature = "cyw43")]
    fn trng_fill(buffer: &mut [u8]) -> bool {
        crate::rp2350_trng::fill(&mut Part, chip::TRNG_BASE as usize, buffer, &mut || clock::now_ms())
    }

    use crate::board_bindings as board;
    use crate::rp2350_clocks;
    use crate::rp2350_instances as chip;

    // --- RP2350 crystal oscillator + peripheral clock. The board's XOSC is 12 MHz; clk_peri is
    //     pointed straight at it (no PLL) so the UART divisor is exact -- and stays exact when
    //     the usb build moves clk_sys onto PLL_SYS. Instance bases and the per-board facts are
    //     generated constants; block offsets and composed words stay in this file. ---
    const XOSC_CTRL: usize = chip::XOSC_BASE as usize;
    const XOSC_STATUS: usize = chip::XOSC_BASE as usize + 0x4;
    const XOSC_STARTUP: usize = chip::XOSC_BASE as usize + 0xc;
    const XOSC_CTRL_ENABLE_1_15MHZ: u32 = 0x00fa_baa0;
    const XOSC_STARTUP_DELAY: u32 = crate::rp2350_xosc_layout::STARTUP_DELAY_RESET;
    const XOSC_STABLE: u32 = 1 << 31;
    const CLK_PERI_CTRL: usize = chip::CLOCKS_BASE as usize + 0x48;
    const CLK_PERI_ENABLE: u32 = 1 << 11;
    const CLK_PERI_AUXSRC_XOSC: u32 = 4 << 5;

    // RESETS: the atomic-clear alias + RESET_DONE compose from the instance bases; the
    // combined uart0+pads_bank0+io_bank0 release word is the board binding's reset mask.
    const RESETS_CLR: usize = chip::RESETS_CLR_BASE as usize;
    const RESETS_DONE: usize = chip::RESETS_BASE as usize + 0x8;
    const RESET_UART0_PADS_IO: u32 = board::UART0_RESET_MASK;

    // IO_BANK0: the binding's resolved per-pin CTRL addresses (GP0 = UART0 TX, GP1 = RX --
    // the Raspberry Pi Debug Probe's UART, bridged to the host COM port) and its function
    // select. The per-pin PADS_BANK0 registers come from the binding too: RP2350 pads reset
    // isolated, so each bound pin's pad register is part of the board binding. The pad words
    // stay in this file (ISO bit 8 clear de-isolates; RX adds IE, bit 6).
    const IO_BANK0_GPIO0_CTRL: usize = board::UART0_IO_TX_CTRL as usize;
    const IO_BANK0_GPIO1_CTRL: usize = board::UART0_IO_RX_CTRL as usize;
    const GPIO_FUNCSEL_UART: u32 = board::UART0_FUNCSEL;
    const PADS_BANK0_GPIO0: usize = board::UART0_PADS_TX as usize;
    const PADS_BANK0_GPIO1: usize = board::UART0_PADS_RX as usize;
    const PAD_TX: u32 = 0x04;
    const PAD_RX_IE: u32 = 0x40;

    // UART0 = PL011 at the binding's base.
    const UART0: usize = board::UART0_BASE as usize;
    const UART_DR: usize = UART0 + 0x00;
    const UART_FR: usize = UART0 + 0x18;
    const UART_IBRD: usize = UART0 + 0x24;
    const UART_FBRD: usize = UART0 + 0x28;
    const UART_LCR_H: usize = UART0 + 0x2c;
    const UART_CR: usize = UART0 + 0x30;
    const FR_TXFF: u32 = 1 << 5;
    const FR_RXFE: u32 = 1 << 4;
    const FR_BUSY: u32 = 1 << 3;
    // 115200 at clk_peri = 12 MHz: 12e6 / (16*115200) = 6.51 -> IBRD 6, FBRD round(.51*64)=33.
    // Generated: the board declares this debug-probe UART as a second carrier paired with its
    // pll-150-48 clock plan, so the divisor pair lives in its board.toml and the generator
    // derives it.
    const IBRD_115200: u32 = board::UART0_IBRD_115200_PLL_150_48;
    const FBRD_115200: u32 = board::UART0_FBRD_115200_PLL_150_48;
    const LCR_H_8N1_FIFO: u32 = 0x70; // WLEN 8-bit (0b11 << 5) | FEN (1 << 4)
    const CR_ENABLE: u32 = 0x301; // UARTEN (1<<0) | TXE (1<<8) | RXE (1<<9)

    /// Point the vector table at the flash base so faults reach `fault` (the bootrom does not
    /// guarantee VTOR).
    const SCB_VTOR: usize = 0xe000_ed08;

    fn clocks_uart_init() {
        // XOSC up.
        write_register(XOSC_STARTUP, XOSC_STARTUP_DELAY);
        write_register(XOSC_CTRL, XOSC_CTRL_ENABLE_1_15MHZ);
        while read_register(XOSC_STATUS) & XOSC_STABLE == 0 {}
        // clk_peri from the XOSC (12 MHz), enabled.
        write_register(CLK_PERI_CTRL, CLK_PERI_ENABLE | CLK_PERI_AUXSRC_XOSC);
        // Un-reset the GPIO mux, pads, and UART0 (the binding's composed word); wait for
        // all three.
        let mask = RESET_UART0_PADS_IO;
        write_register(RESETS_CLR, mask);
        while read_register(RESETS_DONE) & mask != mask {}
        // Route GP0/GP1 to UART0 and de-isolate the pads.
        write_register(IO_BANK0_GPIO0_CTRL, GPIO_FUNCSEL_UART);
        write_register(IO_BANK0_GPIO1_CTRL, GPIO_FUNCSEL_UART);
        write_register(PADS_BANK0_GPIO0, PAD_TX);
        write_register(PADS_BANK0_GPIO1, PAD_RX_IE);
        // PL011 115200 8N1, FIFOs on, TX+RX+UART enabled.
        write_register(UART_IBRD, IBRD_115200);
        write_register(UART_FBRD, FBRD_115200);
        write_register(UART_LCR_H, LCR_H_8N1_FIFO);
        write_register(UART_CR, CR_ENABLE);
    }

    fn uart_tx(byte: u8) {
        while read_register(UART_FR) & FR_TXFF != 0 {}
        write_register(UART_DR, u32::from(byte));
    }

    /// A `core::fmt` sink straight onto the UART, allocating nothing -- which is what lets the
    /// panic handler print its reason even when the panic is an allocation failure. (An
    /// `alloc::format!` there would re-enter the allocator that just refused.)
    ///
    /// The FIFO wait is bounded, unlike [`uart_tx`]: this runs on paths that may precede
    /// `clocks_uart_init` (an early panic), and an unconfigured PL011 never clears TXFF -- an
    /// unbounded spin there would hang the board in the handler whose job is to reset it.
    struct UartWriter;

    impl core::fmt::Write for UartWriter {
        fn write_str(&mut self, text: &str) -> core::fmt::Result {
            for byte in text.bytes() {
                let mut patience = 100_000;
                while read_register(UART_FR) & FR_TXFF != 0 && patience != 0 {
                    patience -= 1;
                }
                write_register(UART_DR, u32::from(byte));
            }
            Ok(())
        }
    }

    /// The `arena-trace` debugging aid, installed as the console tap: each console line goes to the
    /// UART too, ended by the heap's carved, live and free-listed bytes at that moment.
    #[cfg(feature = "arena-trace")]
    fn arena_trace(units: &[u16]) {
        for &unit in units {
            if unit == u16::from(b'\n') {
                let _ = core::fmt::Write::write_fmt(
                    &mut UartWriter,
                    format_args!(
                        " [arena: {} carved, {} live, {} free-listed]\r\n",
                        ALLOCATOR.carved_lockfree(),
                        ALLOCATOR.live_lockfree(),
                        ALLOCATOR.free_list_bytes()
                    ),
                );
            } else if unit != u16::from(b'\r') {
                uart_tx(if unit < 0x80 { unit as u8 } else { b'?' });
            }
        }
    }

    /// Say one line on the UART outside the frame protocol -- the boot path's own voice, for the
    /// moments a framed reply cannot serve: before the transport exists, and from the panic
    /// handler. A host's frame reader resynchronizes on the SYNC magic, so plain text between
    /// frames is discarded rather than mistaken for one.
    fn narrate(text: &str) {
        let _ = core::fmt::Write::write_fmt(&mut UartWriter, format_args!("{text}\r\n"));
    }

    /// Wait for the PL011 to finish shifting out what is queued. The TX FIFO is 32 bytes deep and
    /// `SYSRESETREQ` truncates it mid-character, so a reset that follows narration drains first or
    /// the last line the board says is the one nobody reads. Bounded for the same reason as
    /// [`narrate`].
    fn uart_drain() {
        let mut patience = 1_000_000;
        while read_register(UART_FR) & FR_BUSY != 0 && patience != 0 {
            patience -= 1;
        }
    }

    /// Cortex-M SysTick, free-running on the processor clock: the transport's partial-frame
    /// idle clock. The base build keeps the boot clock (~12 MHz; the full 24-bit reload wraps
    /// ~1.4 s); the usb build runs at 150 MHz and reloads clk_sys/16 so one wrap is 1/16 s.
    const SYST_CSR: usize = 0xe000_e010;
    const SYST_RVR: usize = 0xe000_e014;
    const SYST_CVR: usize = 0xe000_e018;
    const SYST_COUNTFLAG: u32 = 1 << 16;

    fn systick_start(reload: u32) {
        write_register(SYST_RVR, reload);
        write_register(SYST_CVR, 0);
        write_register(SYST_CSR, 0b101); // ENABLE | CLKSOURCE = processor
    }

    /// SysTick wraps of mid-frame silence before a partial frame is dropped -- ~3-4 s in both
    /// builds (the wrap period differs; see `systick_start`).
    #[cfg(not(feature = "usb"))]
    const IDLE_WRAP_LIMIT: u32 = 3;
    #[cfg(feature = "usb")]
    const IDLE_WRAP_LIMIT: u32 = 48;

    /// The lamella-wire byte carrier over UART0: `send` writes an encoded frame; `poll`
    /// drains the PL011 RX FIFO (32 bytes) into the frame reader -- big enough that a command
    /// never overruns between the command and debug loops' polls. A partial frame is dropped
    /// after seconds of line silence (SysTick-clocked) so a host abort cannot wedge the target.
    struct UartTransport {
        reader: FrameReader,
        pending: bool,
        idle_wraps: u32,
    }

    impl UartTransport {
        fn new() -> Self {
            Self { reader: FrameReader::new(), pending: false, idle_wraps: 0 }
        }
    }

    impl Transport for UartTransport {
        fn send(&mut self, msg_type: u8, seq: u16, payload: &[u8]) -> Result<(), TransportError> {
            for byte in encode_frame(msg_type, seq, payload).ok_or(TransportError::PayloadTooLarge)? {
                uart_tx(byte);
            }
            Ok(())
        }

        fn poll(&mut self) -> Result<Option<Frame>, TransportError> {
            let mut received = false;
            while read_register(UART_FR) & FR_RXFE == 0 {
                let byte = read_register(UART_DR) as u8;
                self.reader.push(&[byte]);
                received = true;
            }
            let wrapped = read_register(SYST_CSR) & SYST_COUNTFLAG != 0;
            if received {
                self.pending = true;
                self.idle_wraps = 0;
            } else if self.pending && wrapped {
                self.idle_wraps += 1;
                if self.idle_wraps >= IDLE_WRAP_LIMIT {
                    self.reader = FrameReader::new();
                    self.pending = false;
                    self.idle_wraps = 0;
                }
            }
            let frame = self.reader.next_frame();
            if frame.is_some() {
                self.pending = false;
                self.idle_wraps = 0;
            }
            Ok(frame)
        }
    }

    /// Retained across a warm reset (.noinit, not zeroed by startup): a panic or fault stamps
    /// FAULT_MAGIC here and resets, and the next boot reads it to wait for the host instead of
    /// re-running a deployed app that aborted. Cold-boot garbage almost never equals the magic.
    #[unsafe(link_section = ".noinit")]
    static mut BOOT_FLAG: u32 = 0;
    const FAULT_MAGIC: u32 = 0xB007_5E11;
    const SCB_AIRCR: usize = 0xe000_ed0c;
    const AIRCR_SYSRESETREQ: u32 = 0x05fa_0004; // VECTKEY 0x05FA | SYSRESETREQ (bit 2)

    fn reset_to_serve() -> ! {
        unsafe { core::ptr::write_volatile(&raw mut BOOT_FLAG, FAULT_MAGIC) };
        uart_drain();
        write_register(SCB_AIRCR, AIRCR_SYSRESETREQ);
        loop {}
    }

    /// A clean self-reset into the boot-run path (the host's EXEC of the deployed artifact): clear
    /// the recovery flag so the reboot runs the freshly deployed image (rather than waiting for
    /// the host), then SYSRESETREQ. The EXEC_ACK was flushed before this, so nothing is pending on
    /// the wire.
    fn reset_to_run() -> ! {
        unsafe { core::ptr::write_volatile(&raw mut BOOT_FLAG, 0) };
        uart_drain();
        write_register(SCB_AIRCR, AIRCR_SYSRESETREQ);
        loop {}
    }

    /// Any exception or fault vector lands here: the next boot comes back up waiting for the host.
    ///
    /// It lifts the stack's limit before anything pushes. A fault the limit raised leaves the stack
    /// at its floor, so a handler whose first push went below it would fault again inside the fault
    /// and lock the core up instead of resetting it.
    #[unsafe(no_mangle)]
    #[unsafe(naked)]
    pub extern "C" fn fault() -> ! {
        core::arch::naked_asm!(
            "movs r0, #0",
            "msr MSPLIM, r0",
            "b {recover}",
            recover = sym recover_from_fault,
        )
    }

    extern "C" fn recover_from_fault() -> ! {
        reset_to_serve()
    }

    #[cfg(feature = "cyw43")]
    mod boot_log {
        use core::sync::atomic::{AtomicUsize, Ordering};
        static mut BUF: [u8; 8192] = [0; 8192];
        static LEN: AtomicUsize = AtomicUsize::new(0);

        pub fn push(line: &str) {
            let buf = unsafe { &mut *core::ptr::addr_of_mut!(BUF) };
            let mut at = LEN.load(Ordering::Relaxed);
            for &byte in line.as_bytes().iter().chain(b"\r\n") {
                if at >= buf.len() {
                    break;
                }
                buf[at] = byte;
                at += 1;
            }
            LEN.store(at, Ordering::Relaxed);
        }

        pub fn get() -> &'static str {
            let buf = unsafe { &*core::ptr::addr_of!(BUF) };
            core::str::from_utf8(&buf[..LEN.load(Ordering::Relaxed)]).unwrap_or("")
        }
    }

    static TICKS_PER_MS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(1);

    #[cfg(feature = "resident-corlib")]
    static CORLIB: &[u8] = include_bytes!(env!("LAMELLA_CORLIB_IMAGE"));

    /// The corlib a deployed PE resolves against, or `None` on a build that carries none (which
    /// then takes baked images only). Reached through the board's
    /// [`lamella_runner::FlashSink`], so the boot-run and debug-attach paths cannot disagree
    /// about what this firmware holds.
    pub(crate) fn resident_corlib() -> Option<&'static [u8]> {
        #[cfg(feature = "resident-corlib")]
        {
            Some(CORLIB)
        }
        #[cfg(not(feature = "resident-corlib"))]
        {
            None
        }
    }

    /// Say why the boot path produced no app run, on both voices this board has: the wire's own
    /// terminal event at seq 0 -- the boot path answers no request, so there is no seq to reply to
    /// -- and a plain UART line for a host watching the port rather than speaking the protocol.
    /// Exit 70 is the interpreter's abort convention: nothing the deployed program chose ran to a
    /// value.
    ///
    /// The reason goes on the error stream and the stop event carries the exit code, because a stop
    /// event has no output tail: a host that read the reason out of the terminal frame would stop
    /// receiving it and would not fail -- it would simply see a board that said nothing.
    fn report(transport: &mut impl Transport, reason: &str) {
        use lamella_runner::debug;
        narrate(&alloc::format!("[lamella] {reason}"));
        let mut output = alloc::vec![debug::output::STDERR, 0u8];
        output.extend_from_slice(reason.as_bytes());
        let _ = transport.send(debug::EVT_OUTPUT, 0, &output);
        let mut stopped = alloc::vec![debug::reason::TRAP];
        stopped.extend_from_slice(&0u32.to_le_bytes());
        stopped.extend_from_slice(&0u32.to_le_bytes());
        stopped.extend_from_slice(&70i32.to_le_bytes());
        stopped.push(0);
        let _ = transport.send(debug::EVT_STOPPED, 0, &stopped);
    }

    pub(crate) use crate::systick_clock as clock;

    /// The full name of this board's C# class, in `bsp/<board>/csharp`, which the runtime
    /// initializes before a program's entry point when the program carries it. One bin serves four
    /// boards, so it follows the same feature switch as the board identity in `lamella_main`.
    #[cfg(all(feature = "cyw43", not(feature = "pico-plus-2-w")))]
    const BOARD_CLASS: &str = "Lamella.Boards.RaspberryPi.Pico2W";
    #[cfg(feature = "pico-plus-2-w")]
    const BOARD_CLASS: &str = "Lamella.Boards.Pimoroni.PicoPlus2W";
    #[cfg(feature = "pico-plus-2")]
    const BOARD_CLASS: &str = "Lamella.Boards.Pimoroni.PicoPlus2";
    #[cfg(not(any(feature = "cyw43", feature = "pico-plus-2")))]
    const BOARD_CLASS: &str = "Lamella.Boards.RaspberryPi.Pico2";

    /// Installs the board seams on a fresh evaluation `Vm` (the runner's `_with` hook). Every
    /// build gets the board's clock. A cyw43 build also brings the radio up and joins on the first
    /// evaluation, streams that narration into the run's stdout and -- whenever the radio runs,
    /// joined or not -- wires a smoltcp NetBackend over the chip's ethernet data channel that lends
    /// the radio to the program's Wi-Fi classes, plus the TLS engine in a `tls` build.
    fn configure_vm(vm: &mut lamella_cil_runtime::Vm) {
        vm.set_clock(clock::now_ms, clock::sleep_ms);
        vm.set_board_class(BOARD_CLASS);
        vm.set_pin_event_source(crate::rp2350_pin_events::source_for_a_new_program());
        #[cfg(feature = "arena-trace")]
        vm.set_console_tap(arena_trace);
        #[cfg(feature = "cyw43")]
        {
            use crate::pico_wifi;
            let ticks = TICKS_PER_MS.load(core::sync::atomic::Ordering::Relaxed);
            let settings = alloc::boxed::Box::new(crate::rp2350_flash::SettingsStore::new());
            pico_wifi::ensure_wifi(ticks.saturating_mul(1000), settings, &mut |line: &str| {
                for byte in line.bytes() {
                    uart_tx(byte);
                }
                uart_tx(b'\r');
                uart_tx(b'\n');
                boot_log::push(line);
            });
            let wifi_done = clock::now_ms();
            let units: alloc::vec::Vec<u16> = boot_log::get().encode_utf16().collect();
            vm.write(&units);
            if !pico_wifi::radio_up() {
                let msg: alloc::vec::Vec<u16> =
                    "[net] no radio is running; evaluation runs without a backend\r\n"
                        .encode_utf16()
                        .collect();
                vm.write(&msg);
                return;
            }
            let device = pico_wifi::device();
            let mut config = lamella_net_smoltcp::NetConfig::new(
                pico_wifi::mac(),
                lamella_net_smoltcp::IpSetup::Dhcp,
            );
            config.tcp_rx_buffer = 2048;
            config.tcp_tx_buffer = 2048;
            config.listen_backlog = 1;
            config.would_block_grace_ms = 50;
            let mut seed_bytes = [0u8; 8];
            let seed = if trng_start() && trng_fill(&mut seed_bytes) {
                u64::from_le_bytes(seed_bytes)
            } else {
                let msg: alloc::vec::Vec<u16> =
                    "[net] the TRNG gave no entropy, so the network seed is the MAC and the uptime\r\n"
                        .encode_utf16()
                        .collect();
                vm.write(&msg);
                u64::from(u32::from_le_bytes([
                    pico_wifi::mac()[2],
                    pico_wifi::mac()[3],
                    pico_wifi::mac()[4],
                    pico_wifi::mac()[5],
                ])) ^ clock::now_ms()
            };
            let dhcp_started = clock::now_ms();
            let mut net = lamella_net_smoltcp::SmoltcpNet::new(
                device,
                config,
                clock::now_ms,
                seed,
            )
            .with_wifi(pico_wifi::view);
            let joined = pico_wifi::wifi_up();
            if joined {
                use lamella_cil_runtime::net::NetBackend;
                let deadline = clock::now_ms().saturating_add(8_000);
                while net.ipv4_addr().is_none() && clock::now_ms() < deadline {
                    let _ = net.poll(Some(100));
                }
            }
            let settled = clock::now_ms();
            let to_dhcp_ms = dhcp_started.saturating_sub(wifi_done);
            let dhcp_ms = settled.saturating_sub(dhcp_started);
            let status: alloc::string::String = match net.ipv4_addr() {
                None if !joined => alloc::string::String::from(
                    "[net] the radio is up and no network is joined yet; the address follows the link\r\n",
                ),
                Some(ip) => alloc::format!(
                    "[net] up, address {}.{}.{}.{} from DHCP ({} ms from the wifi step to dhcp start, {} ms more to the lease)\r\n",
                    ip[0], ip[1], ip[2], ip[3], to_dhcp_ms, dhcp_ms
                ),
                None => alloc::format!(
                    "[net] joined, no address YET (dhcp still trying; {} ms from the wifi step to dhcp start, {} ms waited)\r\n",
                    to_dhcp_ms, dhcp_ms
                ),
            };
            let units: alloc::vec::Vec<u16> = status.encode_utf16().collect();
            vm.write(&units);
            vm.set_net_backend(alloc::boxed::Box::new(net));
            #[cfg(feature = "tls")]
            attach_tls(vm);
        }
        #[cfg(not(feature = "cyw43"))]
        let _ = vm;
    }

    /// The TLS engine's wall clock: the managed clock's last set, mirrored.
    #[cfg(feature = "tls")]
    static TLS_CLOCK: crate::tls_clock::Mirror = crate::tls_clock::Mirror::new();

    /// The runtime's wall-clock sink: every set of the managed clock, recorded in [`TLS_CLOCK`].
    #[cfg(feature = "tls")]
    fn tls_clock_set(state: Option<i64>) {
        TLS_CLOCK.set(state, clock::now_ms());
    }

    /// The TLS engine's time source: [`TLS_CLOCK`], advanced to now.
    #[cfg(feature = "tls")]
    fn tls_clock_now() -> u64 {
        TLS_CLOCK.unix_seconds(clock::now_ms())
    }

    /// Installs the on-device TLS engine on a fresh evaluation `Vm`: mbedTLS behind the runtime's TLS
    /// seam, its entropy from the TRNG, its wall clock mirrored from the managed one, and in a
    /// `system-roots` build the bundled CA store, so a session can verify the public web's chains.
    ///
    /// The board has no battery-backed clock. Until a program sets the managed clock, by an SNTP
    /// sync or `SystemClock.Seed`, the engine reads the time as never set and, by the adaptive
    /// default, tolerates a certificate's validity dates and records that it did. From the first set
    /// on, every session checks them in full.
    #[cfg(feature = "tls")]
    fn attach_tls(vm: &mut lamella_cil_runtime::Vm) {
        lamella_tls_mbedtls::set_entropy_source(trng_fill);
        vm.set_wall_clock_sink(tls_clock_set);
        lamella_tls_mbedtls::set_time_source(tls_clock_now);
        #[cfg(feature = "system-roots")]
        lamella_tls_mbedtls::set_system_roots(|issuer, index| lamella_tls_roots::roots_for_issuer(issuer).nth(index));
        let roots = if cfg!(feature = "system-roots") { "the bundled CA store" } else { "pinned certificates only" };
        let line = alloc::format!("[tls] mbedTLS up, entropy from the TRNG, trusting {roots}\r\n");
        let units: alloc::vec::Vec<u16> = line.encode_utf16().collect();
        vm.write(&units);
        vm.set_tls_backend(alloc::boxed::Box::new(lamella_tls_mbedtls::MbedTlsDevice::new()));
    }

    /// The post-startup entry (`startup::reset` has zeroed .bss and copied .data).
    #[unsafe(no_mangle)]
    pub extern "C" fn lamella_main() -> ! {
        write_register(SCB_VTOR, 0x1000_0000);
        guard_stack();
        let (start, bytes) = heap();
        unsafe {
            ALLOCATOR.init(start, bytes);
        }
        clocks_uart_init();
        crate::rp2350_pin_events::init();

        #[cfg(feature = "usb")]
        let clocked = rp2350_usb::clocks_init();
        #[cfg(feature = "usb")]
        systick_start(if clocked { rp2350_usb::CLK_SYS_HZ / 16 } else { 0x00ff_ffff });
        #[cfg(not(feature = "usb"))]
        systick_start(0x00ff_ffff);

        const RP2350_DPIDR: u32 = 0x4c01_3477;
        const SYSINFO_CHIP_ID: usize = 0x4000_0000;
        #[cfg(all(feature = "cyw43", not(feature = "pico-plus-2-w")))]
        let board = lamella_wire::product_model::PICO2_W;
        #[cfg(feature = "pico-plus-2-w")]
        let board = lamella_wire::product_model::PICO_PLUS_2_W;
        #[cfg(feature = "pico-plus-2")]
        let board = lamella_wire::product_model::PICO_PLUS_2;
        #[cfg(not(any(feature = "cyw43", feature = "pico-plus-2")))]
        let board = lamella_wire::product_model::PICO2;
        lamella_runner::set_board_identity(board, RP2350_DPIDR, read_register(SYSINFO_CHIP_ID));

        {
            let tree = rp2350_clocks::read_tree(
                chip::CLOCKS_BASE as usize,
                chip::PLL_SYS_BASE as usize,
                chip::PLL_USB_BASE as usize,
            );
            match rp2350_clocks::clk_sys_hz_from(&tree, board::XOSC_HZ_PLL_150_48) {
                Some(clk_sys_hz) => {
                    TICKS_PER_MS.store(clk_sys_hz / 1000, core::sync::atomic::Ordering::Relaxed);
                    clock::install(clk_sys_hz);
                }
                None => {
                    narrate(&alloc::format!(
                        "[lamella] no clock: clk_sys runs from {}, whose rate is not derivable",
                        rp2350_clocks::clk_sys_source_name(&tree)
                    ));
                }
            }
        }

        let recovered = unsafe {
            let flag = core::ptr::read_volatile(&raw const BOOT_FLAG);
            core::ptr::write_volatile(&raw mut BOOT_FLAG, 0);
            flag == FAULT_MAGIC
        };

        let mut flash = Rp2350Flash::new();

        #[cfg(feature = "usb")]
        let usb_alloc = if clocked { Some(rp2350_usb::UsbBus::new()) } else { None };
        #[cfg(feature = "usb")]
        let mut usb_link = usb_alloc
            .as_ref()
            .map(|alloc| usb_transport::build_device(alloc, "Lamella Link (RP2350)", rp2350_usb::serial_number()));

        let artifact = lamella_runner::FlashSink::image_slice(&flash);
        let deployed = matches!(artifact.get(..2), Some(b"LM") | Some(b"MZ"));
        #[cfg(feature = "usb")]
        if recovered || deployed {
            if let Some((class, dev)) = usb_link.as_mut() {
                let wrap_budget: u32 = if clocked { 32 } else { 2 };
                let mut wraps = 0;
                while dev.state() != usb_device::device::UsbDeviceState::Configured
                    && wraps < wrap_budget
                {
                    let _ = dev.poll(&mut [class]);
                    if read_register(SYST_CSR) & SYST_COUNTFLAG != 0 {
                        wraps += 1;
                    }
                }
            }
        }

        let mut uart_carrier = UartTransport::new();
        #[cfg(feature = "usb")]
        let mut usb_carrier = usb_link.as_mut().map(|(class, dev)| UsbCarrier::new(dev, class));
        let mut carriers = alloc::vec::Vec::new();
        carriers.push(Carrier::physical(&mut uart_carrier));
        #[cfg(feature = "usb")]
        if let Some(usb) = usb_carrier.as_mut() {
            carriers.push(Carrier::physical(usb));
        }
        let mut transport =
            CarrierSet::new(&mut carriers, crate::systick_clock::now_ms, Windows::default())
                .expect("the UART carrier is always present");

        if recovered || deployed {
            if recovered {
                report(
                    &mut transport,
                    "the last run ABORTED THE FIRMWARE (out of memory or a fault); serving instead \
                     of re-running it",
                );
            } else {
                match lamella_runner::load_deployed(artifact, resident_corlib()) {
                    Ok((module, entry)) => {
                        narrate("[lamella] running the deployed artifact");
                        let _ = run_deployed_with(&mut transport, &module, entry, &mut configure_vm);
                        crate::rp2350_pin_events::disarm_every_pin();
                    }
                    Err(reason) => report(&mut transport, &reason),
                }
            }
        }

        let mut load = lamella_runner::ArtifactLoad::new();
        let mut loaded = OneLoadedImage::default();
        loop {
            let served = serve_one_deploy_with_residence(
                &mut transport,
                &mut flash,
                &mut configure_vm,
                None,
                &mut loaded,
                &mut load,
            );
            if !matches!(served, Ok(Served::Nothing)) {
                crate::rp2350_pin_events::disarm_every_pin();
            }
            match served {
                Ok(Served::RunRequested) => reset_to_run(), // EXEC(deployed): boot the deployed image
                Ok(Served::ResetRequested) => reset_to_serve(),
                Ok(Served::Handled | Served::Nothing) | Err(_) => {}
            }
        }
    }

    /// Where a LOADED artifact lives while a plain run or a debug session borrows it: one heap
    /// allocation, freed when the transfer arena lets go of it.
    ///
    /// The runner can only leak an image to give the loader the `'static` bytes it borrows, and with
    /// the arena held across frames every LOAD completes, so leaking would keep one image per run
    /// until the heap ran out. This keeps one at a time.
    #[derive(Default)]
    struct OneLoadedImage {
        /// The image placed last: its pointer, length and capacity, as the `Vec` it arrived in.
        held: Option<(*mut u8, usize, usize)>,
    }

    impl lamella_runner::ImageResidence for OneLoadedImage {
        fn admit(&mut self, image: alloc::vec::Vec<u8>) -> Option<&'static [u8]> {
            lamella_runner::ImageResidence::release(self);
            let mut image = core::mem::ManuallyDrop::new(image);
            let held = (image.as_mut_ptr(), image.len(), image.capacity());
            self.held = Some(held);
            // SAFETY: the allocation is live until `release` rebuilds the Vec it came from and drops
            // it, and `release` runs only when the arena has let go of these bytes with nothing
            // executing -- an execution refuses every transfer while it exists -- so no borrow of
            // the slice outlives the allocation.
            Some(unsafe { core::slice::from_raw_parts(held.0, held.1) })
        }

        fn release(&mut self) {
            if let Some((pointer, len, capacity)) = self.held.take() {
                // SAFETY: the three parts are exactly those of the Vec `admit` took apart, and it was
                // never dropped; the arena has let go of the slice handed out (see `admit`).
                drop(unsafe { alloc::vec::Vec::from_raw_parts(pointer, len, capacity) });
            }
        }
    }
}

#[cfg(not(target_os = "none"))]
fn main() {
    println!(
        "rp2350-csharp-interpreter is a bare-metal target; build for thumbv8m.main-none-eabi with serve,rp2350"
    );
}
