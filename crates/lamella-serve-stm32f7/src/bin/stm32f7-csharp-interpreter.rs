//! The C# interpreter firmware for the STM32F769I-DISCO (STM32F769NI) and the STM32F746G-DISCO
//! (STM32F746NG), both Cortex-M7: the on-device half of a Lamella Link deploy target, taking
//! programs from the lamella CLI over the board's virtual COM port (USART1 at 115200 8N1 through the
//! on-board ST-LINK: TX PA9 on both boards, RX PA10 on the F769 and PB7 on the F746). A program it
//! is given is stored in the part's own flash and runs from reset.
//!
//! Build: cargo build -p lamella-serve-stm32f7 --bin stm32f7-csharp-interpreter --profile flash-speed
//!        --target thumbv7em-none-eabi --features serve,st-stm32f769i-disco
//! Build: cargo build -p lamella-serve-stm32f7 --bin stm32f7-csharp-interpreter --profile flash-speed
//!        --target thumbv7em-none-eabi --features serve,st-stm32f746g-disco
#![cfg_attr(target_os = "none", no_std)]
#![cfg_attr(target_os = "none", no_main)]

#[cfg(all(target_os = "none", not(any(feature = "st-stm32f769i-disco", feature = "st-stm32f746g-disco"))))]
compile_error!("stm32f7-csharp-interpreter needs a board feature: `st-stm32f769i-disco` or `st-stm32f746g-disco`");

#[cfg(all(target_os = "none", feature = "st-stm32f746g-disco", feature = "stm32f769"))]
compile_error!(
    "`st-stm32f746g-disco` links the STM32F746's memory map and `stm32f769`, which \
     `st-stm32f769i-disco` turns on, the STM32F769's: name one board feature"
);

#[cfg(target_os = "none")]
#[path = "../../../lamella-serve-core/src/startup.rs"]
mod startup;

#[cfg(target_os = "none")]
#[path = "../../../lamella-serve-core/src/systick_clock.rs"]
mod systick_clock;

#[cfg(target_os = "none")]
#[path = "../stm32f7_flash.rs"]
mod stm32f7_flash;

#[cfg(target_os = "none")]
#[path = "../../../lamella-serve-core/src/one_loaded_image.rs"]
mod one_loaded_image;

#[cfg(target_os = "none")]
#[path = "../../../lamella-serve-core/src/bare_critical_section.rs"]
mod bare_critical_section;

#[cfg(target_os = "none")]
#[path = "../../../lamella-serve-core/src/pin_events.rs"]
mod pin_events;

#[cfg(target_os = "none")]
#[path = "../../../lamella-serve-core/src/stm32_pin_events.rs"]
mod stm32_pin_events;

#[cfg(all(target_os = "none", feature = "st-stm32f769i-disco"))]
#[allow(dead_code)]
mod chip {
    #[path = "../../../../../csp/stm32f769/rust/stm32f769_instances.rs"]
    pub mod instances;
    #[path = "../../../../../csp/stm32f769/rust/stm32f769_exti_layout.rs"]
    pub mod exti;
    #[path = "../../../../../csp/stm32f769/rust/stm32f769_syscfg_layout.rs"]
    pub mod syscfg;
    #[path = "../../../../../csp/stm32f769/rust/stm32f769_gpio_layout.rs"]
    pub mod gpio;
}

#[cfg(all(target_os = "none", feature = "st-stm32f746g-disco", not(feature = "stm32f769")))]
#[allow(dead_code)]
mod chip {
    #[path = "../../../../../csp/stm32f7/rust/stm32f7_instances.rs"]
    pub mod instances;
    #[path = "../../../../../csp/stm32f7/rust/stm32f7_exti_layout.rs"]
    pub mod exti;
    #[path = "../../../../../csp/stm32f7/rust/stm32f7_syscfg_layout.rs"]
    pub mod syscfg;
    #[path = "../../../../../csp/stm32f7/rust/stm32f7_gpio_layout.rs"]
    pub mod gpio;
}

#[cfg(all(target_os = "none", feature = "st-stm32f769i-disco"))]
#[allow(dead_code)]
#[path = "../../../../bsp/boards/st-stm32f769i-disco/rust/st_stm32f769i_disco_bindings.rs"]
mod board;

#[cfg(all(target_os = "none", feature = "st-stm32f769i-disco"))]
#[allow(dead_code)]
#[path = "../../../../csp/stm32f769/rust/stm32f769_usart_layout.rs"]
mod usart;

#[cfg(all(target_os = "none", feature = "st-stm32f746g-disco", not(feature = "stm32f769")))]
#[allow(dead_code)]
#[path = "../../../../bsp/boards/st-stm32f746g-disco/rust/st_stm32f746g_disco_bindings.rs"]
mod board;

#[cfg(all(target_os = "none", feature = "st-stm32f746g-disco", not(feature = "stm32f769")))]
#[allow(dead_code)]
#[path = "../../../../csp/stm32f7/rust/stm32f7_usart_layout.rs"]
mod usart;

#[cfg(target_os = "none")]
mod serve {
    extern crate alloc;
    use crate::stm32f7_flash::{self, SECTOR_256K, Stm32F7Flash};
    use crate::{board, chip, usart};
    use core::sync::atomic::{AtomicUsize, Ordering};
    use lamella_cil_runtime::Module;
    use lamella_runner::carriers::{Carrier, CarrierSet, Windows};
    use lamella_runner::{FlashSink, Served, run_deployed_with, serve_one_deploy_with_residence};
    use lamella_wire::{Frame, FrameReader, Transport, TransportError, encode_frame};

    /// The interpreter's working memory, a reclaiming heap (lamella-heap), so a program's dropped
    /// allocations return to it and a deployed loop that allocates on every pass runs unbounded:
    /// 384 KB of the F769's 512 KB of SRAM, and 256 KB of the F746's 320 KB. The rest of SRAM holds
    /// the statics and the stack. Chosen by the part feature, as build.rs chooses the memory map, so
    /// the arena is always sized for the RAM the link declares.
    #[cfg(feature = "stm32f769")]
    const ARENA_BYTES: usize = 384 * 1024;
    #[cfg(not(feature = "stm32f769"))]
    const ARENA_BYTES: usize = 256 * 1024;
    #[allow(dead_code)]
    #[repr(align(16))]
    struct Arena([u8; ARENA_BYTES]);
    static mut ARENA: Arena = Arena([0; ARENA_BYTES]);

    #[global_allocator]
    static ALLOCATOR: lamella_heap::LockedHeap = lamella_heap::LockedHeap::empty();

    /// The board's virtual COM port, every address from the board's generated bindings and the
    /// part's USART layout.
    const USART: usize = board::VCP_BASE as usize;
    const USART_CR1: usize = USART + usart::CR1_OFF as usize;
    const USART_BRR: usize = USART + usart::BRR_OFF as usize;
    const USART_ISR: usize = USART + usart::ISR_OFF as usize;
    const USART_ICR: usize = USART + usart::ICR_OFF as usize;
    const USART_RDR: usize = USART + usart::RDR_OFF as usize;
    const USART_TDR: usize = USART + usart::TDR_OFF as usize;
    const CR1_UE: u32 = usart::CR1_UE;
    const CR1_RE: u32 = usart::CR1_RE;
    const CR1_TE: u32 = usart::CR1_TE;
    const CR1_RXNEIE: u32 = usart::CR1_RXNEIE;
    const ISR_RXNE: u32 = usart::ISR_RXNE;
    const ISR_ORE: u32 = usart::ISR_ORE;
    /// Transmission complete: the last frame has left the shift register, where TXE says only that
    /// the transmit register is free for the next byte.
    const ISR_TC: u32 = usart::ISR_TC;
    const ISR_TXE: u32 = usart::ISR_TXE;
    /// USART_ICR bit 3, ORECF (RM0410 Rev 5 section 34.8.9, RM0385 Rev 9 section 31.8.9): writing 1
    /// clears ORE in USART_ISR.
    ///
    /// The only way to clear it on this USART. Reading the status and then the data register, which
    /// clears an overrun on the STM32F4's USART, does not here: RM0410 section 34.8.8 and RM0385
    /// section 31.8.8 say ORE "is cleared by a software, writing 1 to the ORECF, in the USART_ICR
    /// register", and that while RXNEIE is set it raises the interrupt. So a receive handler that
    /// leaves it set is entered again at once, forever.
    const ICR_ORECF: u32 = 1 << 3;
    /// 115200 baud on the 16 MHz HSI this part boots on, as the board's bindings compute it.
    const BRR_115200_AT_16MHZ: u32 = board::VCP_BRR_115200_HSI_16MHZ;
    /// NVIC ISER1 enables interrupts 32 to 63. USART1 is position 37 on both parts (RM0410 Rev 5
    /// Table 46 and RM0385 Rev 9 Table 44, whose rows check themselves: vector address 0x0000 00D4 =
    /// 0x40 + 4 x 37), so its bit is 37 - 32 = 5. The position is stated twice, here and in the
    /// vector slot of each part's memory map, and they move together: changing one alone gives a
    /// board that receives nothing.
    const NVIC_ISER1: usize = 0xE000_E104;
    const USART_IRQ_BIT: u32 = 1 << 5;

    /// The vector table offset register (Armv7-M B3.2.5).
    const SCB_VTOR: usize = 0xE000_ED08;
    const SCB_AIRCR: usize = 0xE000_ED0C;
    const AIRCR_SYSRESETREQ: u32 = 0x05FA_0004; // VECTKEY 0x05FA | SYSRESETREQ (bit 2)
    const SCB_CFSR: usize = 0xE000_ED28;
    const SCB_HFSR: usize = 0xE000_ED2C;
    const SCB_MMFAR: usize = 0xE000_ED34;
    const SCB_BFAR: usize = 0xE000_ED38;

    /// The core clock SysTick counts, and the scale of the interpreter's millisecond clock: the
    /// 16 MHz HSI reset clock, which this firmware does not change.
    const CPU_HZ: u32 = 16_000_000;

    unsafe extern "C" {
        /// The first word of this firmware's vector table (the part's memory map).
        static _svector_table: u32;
    }

    fn write_reg(addr: usize, val: u32) {
        unsafe { core::ptr::write_volatile(addr as *mut u32, val) };
    }

    fn read_reg(addr: usize) -> u32 {
        unsafe { core::ptr::read_volatile(addr as *const u32) }
    }

    /// Set the bits `mask` selects in `reg` to `value`, leaving the rest alone: the shape every
    /// generated MODER and AFR fact comes in.
    fn apply(reg: u32, mask: u32, value: u32) {
        let reg = reg as usize;
        write_reg(reg, (read_reg(reg) & !mask) | value);
    }

    /// Set the bits `mask` selects: the shape every generated RCC enable comes in.
    fn set_bits(reg: u32, mask: u32) {
        let reg = reg as usize;
        write_reg(reg, read_reg(reg) | mask);
    }

    fn uart_init(brr: u32) {
        set_bits(board::VCP_TX_PORT_RCC_EN_REG, board::VCP_TX_PORT_RCC_EN_MASK);
        set_bits(board::VCP_RX_PORT_RCC_EN_REG, board::VCP_RX_PORT_RCC_EN_MASK);
        set_bits(board::VCP_RCC_EN_REG, board::VCP_RCC_EN_MASK);

        apply(board::VCP_TX_MODER_REG, board::VCP_TX_MODER_MASK, board::VCP_TX_MODER_VALUE);
        apply(board::VCP_TX_AFR_REG, board::VCP_TX_AFR_MASK, board::VCP_TX_AFR_VALUE);
        apply(board::VCP_RX_MODER_REG, board::VCP_RX_MODER_MASK, board::VCP_RX_MODER_VALUE);
        apply(board::VCP_RX_AFR_REG, board::VCP_RX_AFR_MASK, board::VCP_RX_AFR_VALUE);

        write_reg(USART_BRR, brr);
        write_reg(USART_CR1, CR1_TE | CR1_RE | CR1_RXNEIE);
        write_reg(USART_CR1, CR1_TE | CR1_RE | CR1_RXNEIE | CR1_UE);
        write_reg(NVIC_ISER1, USART_IRQ_BIT);
    }

    /// Send one byte, waiting a bounded time for the transmit register.
    ///
    /// Every reply and every diagnostic passes through here, so a transmitter that never reports
    /// ready must cost a byte rather than the board: an unbounded wait would leave it silent, which a
    /// host cannot tell from a crash. The bound is generous next to one character at 115200 baud
    /// (about 87 us) at 16 MHz.
    fn uart_tx(byte: u8) {
        let mut patience = 100_000;
        while read_reg(USART_ISR) & ISR_TXE == 0 {
            patience -= 1;
            if patience == 0 {
                return;
            }
        }
        write_reg(USART_TDR, u32::from(byte));
    }

    /// Wait, bounded, until the last byte has left the transmitter, so a reset that follows does not
    /// cut a diagnostic off on the wire.
    fn uart_drain() {
        let mut patience = 200_000;
        while read_reg(USART_ISR) & ISR_TC == 0 {
            patience -= 1;
            if patience == 0 {
                return;
            }
        }
    }

    /// A string over the UART, allocation-free: an out-of-memory abort must not ask the allocator
    /// that just refused for room to describe itself.
    fn uart_str(text: &str) {
        for byte in text.bytes() {
            uart_tx(byte);
        }
    }

    /// `label` then `value` as eight lowercase hex digits, allocation-free for the same reason.
    fn uart_hex(label: &str, value: u32) {
        uart_str(label);
        let mut digit = 8;
        while digit > 0 {
            digit -= 1;
            let nibble = ((value >> (digit * 4)) & 0xf) as u8;
            uart_tx(if nibble < 10 { b'0' + nibble } else { b'a' + nibble - 10 });
        }
    }

    /// A `core::fmt` sink over the UART, so a panic's own message is written without a heap.
    struct UartWriter;

    impl core::fmt::Write for UartWriter {
        fn write_str(&mut self, text: &str) -> core::fmt::Result {
            uart_str(text);
            Ok(())
        }
    }

    /// A firmware-level abort -- an allocation the arena could not satisfy, or a bug -- says which,
    /// then resets into serve. A safe interpreted program's own trap does not reach here:
    /// `run_deployed_with` returns it.
    #[panic_handler]
    fn panic(info: &core::panic::PanicInfo) -> ! {
        let _ = core::fmt::Write::write_fmt(&mut UartWriter, format_args!("[lamella] FIRMWARE ABORT: {info}\r\n"));
        reset_to_serve()
    }

    /// Cortex-M SysTick, free-running on the core clock with its full 24-bit reload: one wrap about
    /// every 1.05 s at 16 MHz. COUNTFLAG (CSR bit 16) reads 1 once per wrap, the partial-frame idle
    /// clock the transport counts.
    const SYST_CSR: usize = 0xE000_E010;
    const SYST_RVR: usize = 0xE000_E014;
    const SYST_CVR: usize = 0xE000_E018;
    const SYST_ENABLE_CPU_CLOCK: u32 = 0b101;
    const SYST_COUNTFLAG: u32 = 1 << 16;
    const SYST_RELOAD_FULL: u32 = 0x00FF_FFFF;

    fn systick_start(reload: u32) {
        write_reg(SYST_RVR, reload);
        write_reg(SYST_CVR, 0);
        write_reg(SYST_CSR, SYST_ENABLE_CPU_CLOCK);
    }

    /// The board seams a fresh evaluation's [`lamella_cil_runtime::Vm`] gets (the runner's `_with`
    /// hook): the clock, without which the scheduler computes a `Thread.Sleep` deadline that has
    /// already passed and the sleep returns at once, and the pin-event queue, with every GPIO line
    /// disarmed and the queue emptied first, so the program hears only the pins it arms.
    fn configure_vm(vm: &mut lamella_cil_runtime::Vm) {
        vm.set_clock(crate::systick_clock::now_ms, crate::systick_clock::sleep_ms);
        vm.set_pin_event_source(crate::stm32_pin_events::source_for_a_new_program(&LINES));
    }

    /// The external interrupt controller's GPIO lines, every address from the part's generated
    /// layouts: one pending register for both edges, and each line's port selected in SYSCFG.
    const LINES: crate::stm32_pin_events::Lines = crate::stm32_pin_events::Lines {
        mask: chip::instances::EXTI_BASE + chip::exti::IMR_OFF,
        rising_trigger: chip::instances::EXTI_BASE + chip::exti::RTSR_OFF,
        falling_trigger: chip::instances::EXTI_BASE + chip::exti::FTSR_OFF,
        rising_pending: chip::instances::EXTI_BASE + chip::exti::PR_OFF,
        falling_pending: chip::instances::EXTI_BASE + chip::exti::PR_OFF,
        selectors: chip::instances::SYSCFG_BASE + chip::syscfg::EXTICR1_OFF,
        selector_bits: chip::syscfg::EXTICR1_EXTI1_LSB - chip::syscfg::EXTICR1_EXTI0_LSB,
        ports: &[
            chip::instances::GPIOA_BASE,
            chip::instances::GPIOB_BASE,
            chip::instances::GPIOC_BASE,
            chip::instances::GPIOD_BASE,
            chip::instances::GPIOE_BASE,
            chip::instances::GPIOF_BASE,
            chip::instances::GPIOG_BASE,
            chip::instances::GPIOH_BASE,
            chip::instances::GPIOI_BASE,
            chip::instances::GPIOJ_BASE,
            chip::instances::GPIOK_BASE,
        ],
        idr: chip::gpio::IDR_OFF,
    };

    /// The vector positions the GPIO lines interrupt at, the same on both parts (RM0410 Rev 5 Table
    /// 46, RM0385 Rev 9 Table 44): EXTI0 to EXTI4 at 6 to 10, EXTI9_5 at 23 and EXTI15_10 at 40. The
    /// memory map gives each the handler below, and the two move together: a position enabled here
    /// with no handler in its slot would fetch a zero vector.
    const EXTI_VECTORS: [u32; 7] = [6, 7, 8, 9, 10, 23, 40];

    /// The handler at every GPIO vector (`memory-stm32f769.x`, `memory-stm32f7.x`): each pending line
    /// cleared, then queued as its pin and the level its pad reads.
    #[unsafe(no_mangle)]
    pub extern "C" fn exti_isr() {
        crate::stm32_pin_events::answer(&LINES);
    }

    const RX_RING: usize = 512;
    static mut RX_BUF: [u8; RX_RING] = [0; RX_RING];
    static RX_HEAD: AtomicUsize = AtomicUsize::new(0);
    static RX_TAIL: AtomicUsize = AtomicUsize::new(0);

    /// The USART1 interrupt handler, named by the vector table (position 37 in each part's memory
    /// map).
    ///
    /// Reading RDR clears RXNE (RM0410 Rev 5 section 34.8.8, RM0385 Rev 9 section 31.8.8); an
    /// overrun is cleared through ICR, the only way this USART clears one. The byte RDR holds when
    /// an overrun is flagged is intact (the shift register's is the one lost), so it is still
    /// taken.
    #[unsafe(no_mangle)]
    pub extern "C" fn usart1_isr() {
        loop {
            let status = read_reg(USART_ISR);
            if status & ISR_ORE != 0 {
                write_reg(USART_ICR, ICR_ORECF);
            }
            if status & ISR_RXNE == 0 {
                break;
            }
            let byte = read_reg(USART_RDR) as u8;
            let head = RX_HEAD.load(Ordering::Relaxed);
            let next = (head + 1) % RX_RING;
            if next != RX_TAIL.load(Ordering::Acquire) {
                unsafe {
                    core::ptr::write_volatile(core::ptr::addr_of_mut!(RX_BUF).cast::<u8>().add(head), byte);
                }
                RX_HEAD.store(next, Ordering::Release);
            }
        }
    }

    fn rx_pop() -> Option<u8> {
        let tail = RX_TAIL.load(Ordering::Relaxed);
        if tail == RX_HEAD.load(Ordering::Acquire) {
            return None;
        }
        let byte = unsafe { core::ptr::read_volatile(core::ptr::addr_of!(RX_BUF).cast::<u8>().add(tail)) };
        RX_TAIL.store((tail + 1) % RX_RING, Ordering::Release);
        Some(byte)
    }

    /// The lamella-wire byte carrier over the board's virtual COM port: `send` writes one encoded
    /// frame; `poll` drains the receive ring into the self-synchronizing frame reader. A partial
    /// frame whose remainder never arrives, as when a host abandons a send, is discarded after about
    /// 3 s of line silence.
    struct UsartTransport {
        reader: FrameReader,
        pending: bool,
        idle_wraps: u32,
    }

    impl UsartTransport {
        fn new() -> Self {
            Self { reader: FrameReader::new(), pending: false, idle_wraps: 0 }
        }
    }

    /// SysTick wraps of mid-frame silence before a partial frame is dropped: about 3 s.
    const IDLE_WRAP_LIMIT: u32 = 3;

    impl Transport for UsartTransport {
        /// What the receive ring holds, one slot less than its length: the interrupt moves each byte into
        /// it as it lands, so a frame that fits arrives whole however long the serve loop is busy.
        fn max_inbound_payload(&self) -> Option<usize> {
            Some(lamella_wire::max_payload_for_ring(RX_RING))
        }

        fn send(&mut self, msg_type: u8, seq: u16, payload: &[u8]) -> Result<(), TransportError> {
            for byte in encode_frame(msg_type, seq, payload).ok_or(TransportError::PayloadTooLarge)? {
                uart_tx(byte);
            }
            Ok(())
        }

        fn poll(&mut self) -> Result<Option<Frame>, TransportError> {
            let mut received = false;
            while let Some(byte) = rx_pop() {
                self.reader.push(&[byte]);
                received = true;
            }
            let wrapped = read_reg(SYST_CSR) & SYST_COUNTFLAG != 0;
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

    /// Retained across a warm reset (`.noinit`, which startup does not zero): a panic or fault stamps
    /// [`FAULT_MAGIC`] here and resets, and the next boot reads it to serve instead of running again
    /// the deployed program that aborted. Cold-boot SRAM almost never holds the magic by chance.
    #[unsafe(link_section = ".noinit")]
    static mut BOOT_FLAG: u32 = 0;
    const FAULT_MAGIC: u32 = 0xB007_5E11;

    /// Leave `flag` for the next boot, let the transmitter finish, and reset.
    fn reset_with(flag: u32) -> ! {
        unsafe { core::ptr::write_volatile(&raw mut BOOT_FLAG, flag) };
        uart_drain();
        unsafe { core::arch::asm!("dsb", options(nostack, preserves_flags)) };
        write_reg(SCB_AIRCR, AIRCR_SYSRESETREQ);
        loop {
            core::hint::spin_loop();
        }
    }

    fn reset_to_serve() -> ! {
        reset_with(FAULT_MAGIC)
    }

    /// A clean self-reset into the boot path (the host's DEPLOY_RUN): the flag is cleared, so the
    /// next boot runs the freshly deployed image instead of serving.
    fn reset_to_run() -> ! {
        reset_with(0)
    }

    /// The deploy window in the part's internal flash, written through the F7 flash controller.
    ///
    /// The window (the part's last sectors: 8 to 11 on the F769, 6 and 7 on the F746) is disjoint
    /// from the firmware, which the memory map keeps below it, so an erase or a program only stalls
    /// the core's next fetch until it completes (section 3.3.3 of RM0410 and of RM0385); nothing
    /// erases the code that is running. Every write and erase is refused, and nothing is erased, on
    /// a part whose sectors at these addresses are not this build's map: an F769 in dual-bank mode,
    /// or, under the F746's build, any part but a 1-Mbyte STM32F74x/F75x.
    struct FlashDeploy;

    impl FlashDeploy {
        /// The window's sectors that hold bytes `offset..end` of the window, or `None` when the part's
        /// sectors are not this build's map, `offset` is not on a sector boundary, the range is empty,
        /// or the window is not where this map can erase it.
        fn sectors(offset: usize, end: usize) -> Option<core::ops::Range<u32>> {
            if !Stm32F7Flash::erases_by_this_map() || offset % SECTOR_256K != 0 || end <= offset {
                return None;
            }
            let reach = Stm32F7Flash::window_sectors(end);
            let first = reach.start + (offset / SECTOR_256K) as u32;
            (first < reach.end).then_some(first..reach.end)
        }

        /// The window's bytes `offset..offset + len`, as the flash reads now.
        fn window(offset: usize, len: usize) -> &'static [u8] {
            unsafe { core::slice::from_raw_parts((stm32f7_flash::image_base() + offset) as *const u8, len) }
        }

        /// Erase `sectors`, and say whether every erase was clean and the sectors now read erased.
        fn erase_sectors(sectors: core::ops::Range<u32>, offset: usize) -> bool {
            let len = sectors.len() * SECTOR_256K;
            let mut flash = Stm32F7Flash;
            flash.unlock();
            let mut clean = true;
            for sector in sectors {
                if !flash.erase_sector(sector) {
                    clean = false;
                    break;
                }
            }
            flash.lock();
            clean && Self::window(offset, len).iter().all(|&byte| byte == 0xFF)
        }

        /// Program `bytes` at `offset` in the window, and say whether the controller reported no
        /// error and the bytes read back.
        fn program_at(offset: usize, bytes: &[u8]) -> bool {
            let mut flash = Stm32F7Flash;
            flash.unlock();
            let clean = flash.program_bytes(stm32f7_flash::image_base() + offset, bytes);
            flash.lock();
            clean && Self::window(offset, bytes.len()) == bytes
        }
    }

    impl FlashSink for FlashDeploy {
        fn image_slice(&self) -> &'static [u8] {
            Self::window(0, stm32f7_flash::image_len())
        }

        fn erase(&mut self) {
            if let Some(first) = Self::sectors(0, SECTOR_256K) {
                let _ = Self::erase_sectors(first, 0);
            }
        }

        fn erase_unit(&self) -> Option<usize> {
            Some(SECTOR_256K)
        }

        fn erase_range(&mut self, offset: usize, len: usize) -> bool {
            let in_window = offset.checked_add(len).is_some_and(|end| end <= stm32f7_flash::image_len());
            if !in_window || len % SECTOR_256K != 0 {
                return false;
            }
            match Self::sectors(offset, offset + len) {
                Some(sectors) => Self::erase_sectors(sectors, offset),
                None => false,
            }
        }

        fn program(&mut self, image: &[u8]) -> bool {
            if image.is_empty() || image.len() > stm32f7_flash::image_len() {
                return false;
            }
            match Self::sectors(0, image.len()) {
                Some(sectors) => Self::erase_sectors(sectors, 0) && Self::program_at(0, image),
                None => false,
            }
        }

        fn program_chunk(&mut self, offset: usize, chunk: &[u8], total: usize) -> bool {
            let Some(end) = offset.checked_add(chunk.len()) else {
                return false;
            };
            if total > stm32f7_flash::image_len() || end > total || offset % 4 != 0 {
                return false;
            }
            if !Stm32F7Flash::erases_by_this_map() || Stm32F7Flash::window_sectors(total).is_empty() {
                return false;
            }
            let erase_from = offset.next_multiple_of(SECTOR_256K);
            let erase_to = end.next_multiple_of(SECTOR_256K);
            if erase_to > erase_from && !Self::window(erase_from, erase_to - erase_from).iter().all(|&byte| byte == 0xFF) {
                let erased = match Self::sectors(erase_from, erase_to) {
                    Some(sectors) => Self::erase_sectors(sectors, erase_from),
                    None => false,
                };
                if !erased {
                    return false;
                }
            }
            Self::program_at(offset, chunk)
        }
    }

    /// Any exception or fault vector lands here: say which fault and where, then reset into serve.
    ///
    /// The status registers are architectural (Armv7-M B3.2): CFSR says why -- a bus fault, an
    /// unaligned access, an undefined instruction -- and the address registers say where when their
    /// valid bits are set. The exception number comes from IPSR, so a fault taken inside an
    /// interrupt is told apart from one on the main path.
    #[unsafe(no_mangle)]
    pub extern "C" fn fault() -> ! {
        let ipsr: u32;
        unsafe { core::arch::asm!("mrs {0}, ipsr", out(reg) ipsr) };
        uart_str("[lamella] FAULT");
        uart_hex(" exc=", ipsr & 0x1ff);
        uart_hex(" cfsr=", read_reg(SCB_CFSR));
        uart_hex(" hfsr=", read_reg(SCB_HFSR));
        uart_hex(" mmfar=", read_reg(SCB_MMFAR));
        uart_hex(" bfar=", read_reg(SCB_BFAR));
        uart_str("\r\n");
        reset_to_serve()
    }

    /// The post-startup entry (`startup::reset` has zeroed .bss and copied .data).
    #[unsafe(no_mangle)]
    pub extern "C" fn lamella_main() -> ! {
        write_reg(SCB_VTOR, (&raw const _svector_table) as u32);
        unsafe {
            ALLOCATOR.init(core::ptr::addr_of_mut!(ARENA).cast::<u8>(), ARENA_BYTES);
        }
        uart_init(BRR_115200_AT_16MHZ);
        systick_start(SYST_RELOAD_FULL);
        let _ = crate::systick_clock::install(CPU_HZ);
        crate::stm32_pin_events::init(&LINES, &EXTI_VECTORS);

        lamella_runner::set_board_identity(board::BOARD_MODEL, 0, read_reg(stm32f7_flash::DBGMCU_IDCODE));

        let recovered = unsafe {
            let flag = core::ptr::read_volatile(&raw const BOOT_FLAG);
            core::ptr::write_volatile(&raw mut BOOT_FLAG, 0);
            flag == FAULT_MAGIC
        };

        let mut flash = FlashDeploy;
        let boot_image = if recovered {
            None
        } else {
            match Module::from_baked(flash.image_slice()) {
                Ok((module, Some(entry))) => Some((module, entry)),
                _ => None,
            }
        };

        let mut usart_carrier = UsartTransport::new();
        let mut carriers = alloc::vec::Vec::new();
        carriers.push(Carrier::physical(&mut usart_carrier));
        let mut transport = CarrierSet::new(&mut carriers, crate::systick_clock::now_ms, Windows::default())
            .expect("the USART carrier is always present");

        if let Some((module, entry)) = boot_image {
            let _ = run_deployed_with(&mut transport, &module, entry, &mut configure_vm);
            crate::stm32_pin_events::disarm_every_line(&LINES);
        }

        // SAFETY: the residence goes only to `serve_one_deploy_with_residence` below, beside `load`,
        // and is made first so that it would be dropped after it; the loop never ends, so neither is.
        let mut loaded = unsafe { crate::one_loaded_image::OneLoadedImage::new() };
        let mut load = lamella_runner::ArtifactLoad::new();
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
                crate::stm32_pin_events::disarm_every_line(&LINES);
            }
            match served {
                Ok(Served::RunRequested) => reset_to_run(),
                Ok(Served::ResetRequested) => reset_to_serve(),
                Ok(Served::Handled | Served::Nothing) | Err(_) => {}
            }
        }
    }
}

#[cfg(not(target_os = "none"))]
fn main() {
    println!(
        "stm32f7-csharp-interpreter is a bare-metal target; build for thumbv7em-none-eabi with serve and one board \
         feature, st-stm32f769i-disco or st-stm32f746g-disco"
    );
}
