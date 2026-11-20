//! The C# interpreter firmware for the NUCLEO-U5A5ZJ-Q (STM32U5A5ZJ, Cortex-M33): the on-device
//! half of a Lamella Link deploy target, taking programs from the lamella CLI over the board's
//! virtual COM port (USART1, TX PA9 / RX PA10, 115200 8N1 through the on-board ST-LINK). A program
//! it is given is stored in the part's own flash and runs from reset.
//!
//! Build: cargo build -p lamella-serve-stm32u5 --bin stm32u5-csharp-interpreter --profile flash-speed
//!        --target thumbv8m.main-none-eabi --features serve,st-nucleo-u5a5zj-q
#![cfg_attr(target_os = "none", no_std)]
#![cfg_attr(target_os = "none", no_main)]

#[cfg(all(target_os = "none", not(feature = "st-nucleo-u5a5zj-q")))]
compile_error!("stm32u5-csharp-interpreter needs a board feature: `st-nucleo-u5a5zj-q`");

#[cfg(target_os = "none")]
#[path = "../../../lamella-serve-core/src/startup.rs"]
mod startup;

#[cfg(target_os = "none")]
#[path = "../../../lamella-serve-core/src/systick_clock.rs"]
mod systick_clock;

#[cfg(target_os = "none")]
#[path = "../stm32u5_flash.rs"]
mod stm32u5_flash;

#[cfg(target_os = "none")]
#[path = "../stm32u5_deploy.rs"]
mod stm32u5_deploy;

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

#[cfg(target_os = "none")]
#[allow(dead_code)]
mod chip {
    #[path = "../../../../../csp/stm32u5a5/rust/stm32u5a5_exti_layout.rs"]
    pub mod exti;
    #[path = "../../../../../csp/stm32u5a5/rust/stm32u5a5_gpio_layout.rs"]
    pub mod gpio;
}

#[cfg(all(target_os = "none", feature = "st-nucleo-u5a5zj-q"))]
#[allow(dead_code)]
#[path = "../../../../bsp/boards/st-nucleo-u5a5zj-q/rust/st_nucleo_u5a5zj_q_bindings.rs"]
mod board;

#[cfg(target_os = "none")]
#[allow(dead_code)]
#[path = "../../../../csp/stm32u5a5/rust/stm32u5a5_instances.rs"]
mod instances;

#[cfg(target_os = "none")]
mod serve {
    extern crate alloc;
    use crate::{board, chip, instances};
    use crate::stm32u5_deploy;
    use crate::stm32u5_flash::{self, PAGE, QUAD_WORD, Stm32U5Flash};
    use core::sync::atomic::{AtomicUsize, Ordering};
    use lamella_cil_runtime::Module;
    use lamella_runner::carriers::{Carrier, CarrierSet, Windows};
    use lamella_runner::{FlashSink, Served, run_deployed_with, serve_one_deploy_with_residence};
    use lamella_wire::{Frame, FrameReader, Transport, TransportError, encode_frame};

    unsafe extern "C" {
        /// The first word of this firmware's vector table (`memory-stm32u5a5.x`).
        static _svector_table: u32;
        /// The first byte past the statics (`memory-stm32u5a5.x`), where the heap starts.
        static mut _sheap: u8;
        /// The stack's floor (`memory-stm32u5a5.x`): where the heap ends, and the lowest address the
        /// stack may reach.
        static _stack_floor: u8;
    }

    /// The interpreter's working memory: every byte of SRAM between the statics and the stack's
    /// floor, as the link laid it out, about 2.3 MB of the part's 2,496 KB. A reclaiming heap
    /// (lamella-heap), so a program's dropped allocations return to it and a deployed loop that
    /// allocates on every pass runs unbounded.
    fn heap() -> (*mut u8, usize) {
        let start = &raw mut _sheap;
        let end = (&raw const _stack_floor) as usize;
        (start, end - start as usize)
    }

    /// Hands the core the stack's floor (ARMv8-M `MSPLIM`), so a stack that outgrows its share
    /// faults instead of writing into the heap below it.
    fn guard_stack() {
        let floor = (&raw const _stack_floor) as u32;
        unsafe { core::arch::asm!("msr MSPLIM, {0}", in(reg) floor, options(nomem, nostack, preserves_flags)) };
    }

    #[global_allocator]
    static ALLOCATOR: lamella_heap::LockedHeap = lamella_heap::LockedHeap::empty();

    /// The board's virtual COM port, at the base its generated bindings give. The register offsets
    /// and bits are RM0456 Rev 7 section 66.8's for this USART with its FIFO off, its reset state,
    /// and they are the STM32F7's exactly.
    const USART: usize = board::VCP_BASE as usize;
    /// USART_CR1 [alternate], offset 0x00 (section 66.8.2): UE bit 0, RE bit 2, TE bit 3, RXNEIE
    /// bit 5.
    const USART_CR1: usize = USART;
    const CR1_UE: u32 = 1 << 0;
    const CR1_RE: u32 = 1 << 2;
    const CR1_TE: u32 = 1 << 3;
    const CR1_RXNEIE: u32 = 1 << 5;
    /// USART_BRR, offset 0x0C (section 66.8.6), written only while UE is clear.
    const USART_BRR: usize = USART + 0x0C;
    /// USART_ISR [alternate], offset 0x1C (section 66.8.11): ORE bit 3, RXNE bit 5, TC bit 6, TXE
    /// bit 7.
    const USART_ISR: usize = USART + 0x1C;
    const ISR_ORE: u32 = 1 << 3;
    const ISR_RXNE: u32 = 1 << 5;
    /// Transmission complete: the last frame has left the shift register, where TXE says only that
    /// the transmit register is free for the next byte.
    const ISR_TC: u32 = 1 << 6;
    const ISR_TXE: u32 = 1 << 7;
    /// USART_ICR, offset 0x20 (section 66.8.12).
    const USART_ICR: usize = USART + 0x20;
    /// USART_ICR bit 3, ORECF: writing 1 clears ORE in USART_ISR.
    ///
    /// The only way to clear it on this USART. Section 66.8.11 says ORE "is cleared by a software,
    /// writing 1 to the ORECF, in the USART_ICR register", and that while RXNEIE is set it raises
    /// the interrupt. So a receive handler that leaves it set is entered again at once, forever.
    const ICR_ORECF: u32 = 1 << 3;
    /// USART_RDR, offset 0x24 (section 66.8.13), and USART_TDR, offset 0x28 (section 66.8.14).
    const USART_RDR: usize = USART + 0x24;
    const USART_TDR: usize = USART + 0x28;
    /// 115200 baud at the 16 MHz this firmware runs the part at, 16x oversampling:
    /// 16e6 / 115200 = 138.9, so 139, and the line runs 0.08% slow. USART1's kernel clock is PCLK2
    /// at reset (RCC_CCIPR1 USART1SEL = 00), which is the system clock while the AHB and APB2
    /// prescalers stay at 1.
    const BRR_115200_AT_16MHZ: u32 = 139;
    /// NVIC ISER1 enables interrupts 32 to 63. USART1 is position 61 (RM0456 Rev 7 Table 186, whose
    /// row checks itself: vector address 0x0000 0134 = 0x40 + 4 x 61), so its bit is 61 - 32 = 29.
    /// The position is stated twice, here and in the vector slot of `memory-stm32u5a5.x`, and the
    /// two move together: changing one alone gives a board that receives nothing.
    const NVIC_ISER1: usize = 0xE000_E104;
    const USART_IRQ_BIT: u32 = 1 << 29;

    /// The two RCC registers the clock routine uses: RCC_CR at offset 0x000 (RM0456 section
    /// 11.8.1), MSISRDY bit 2; RCC_ICSCR1 at offset 0x008 (section 11.8.2), MSISRANGE bits 31:28 and
    /// MSIRGSEL bit 23.
    const RCC: usize = instances::RCC_BASE as usize;
    const RCC_CR: usize = RCC;
    const CR_MSISRDY: u32 = 1 << 2;
    const RCC_ICSCR1: usize = RCC + 0x008;
    const ICSCR1_MSISRANGE_MASK: u32 = 0xF << 28;
    /// MSISRANGE 0010, "range 2 around 16 MHz".
    const ICSCR1_MSISRANGE_16MHZ: u32 = 0b0010 << 28;
    /// MSIRGSEL = 1 takes the MSIS and MSIK ranges from this register rather than from RCC_CSR,
    /// where they come from after a reset. MSIKRANGE here resets to 4 MHz, MSIK's reset range, so
    /// only MSIS changes.
    const ICSCR1_MSIRGSEL: u32 = 1 << 23;

    /// The vector table offset register (Armv8-M), at its architectural address.
    const SCB_VTOR: usize = 0xE000_ED08;
    const SCB_AIRCR: usize = 0xE000_ED0C;
    const AIRCR_SYSRESETREQ: u32 = 0x05FA_0004; // VECTKEY 0x05FA | SYSRESETREQ (bit 2)
    const SCB_CFSR: usize = 0xE000_ED28;
    const SCB_HFSR: usize = 0xE000_ED2C;
    const SCB_MMFAR: usize = 0xE000_ED34;
    const SCB_BFAR: usize = 0xE000_ED38;

    /// The die identity the part reports about itself (RM0456 Rev 7 section 75.12.4): the DBGMCU
    /// registers are accessible "to software, at base address 0xE004 4000", and DBGMCU_IDCODE is at
    /// offset 0x00, with `DEV_ID` in bits 11:0 and `REV_ID` in 31:16.
    const DBGMCU_IDCODE: usize = 0xE004_4000;

    /// The core clock SysTick counts, and the scale of the interpreter's millisecond clock: MSIS
    /// as `clock_init_16mhz` leaves it.
    const CPU_HZ: u32 = 16_000_000;

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

    /// Set the bits `mask` selects in `reg`, leaving the rest alone.
    fn set_bits(reg: u32, mask: u32) {
        let reg = reg as usize;
        write_reg(reg, read_reg(reg) | mask);
    }

    /// Turn on a peripheral's clock, the shape every generated RCC enable comes in, and read the
    /// enable register back before returning. RM0456 Rev 7 section 11.4.24: the clock is active two
    /// cycles of the peripheral bus clock after the enable bit is set, and software "must wait for
    /// these two clock cycles before accessing the peripheral registers". The read cannot complete
    /// before the write it follows has reached RCC.
    fn enable_clock(reg: u32, mask: u32) {
        set_bits(reg, mask);
        let _ = read_reg(reg as usize);
    }

    /// Raise MSIS from its 4 MHz reset range to 16 MHz, by RM0456 Rev 7's own order. One wait state
    /// first, read back in force (section 7.3.3): after reset the regulator is the LDO in range 4,
    /// where Table 54 allows 12 MHz at zero wait states and 25 MHz at one. Then the range, which may
    /// be changed only while MSIS is off or ready (section 11.8.2), so the routine waits for ready
    /// before the write and again after it. The voltage range stays as reset left it.
    fn clock_init_16mhz() {
        stm32u5_flash::set_wait_states(1);
        while read_reg(RCC_CR) & CR_MSISRDY == 0 {}
        let icscr1 = read_reg(RCC_ICSCR1);
        write_reg(RCC_ICSCR1, (icscr1 & !ICSCR1_MSISRANGE_MASK) | ICSCR1_MSISRANGE_16MHZ | ICSCR1_MSIRGSEL);
        while read_reg(RCC_CR) & CR_MSISRDY == 0 {}
    }

    /// Validate the VDDIO2 supply, so a program can drive the pins it powers (PG2 to PG15, among
    /// them the board's LD3 on PG2). The part isolates that rail at reset; the board's bindings give
    /// the PWR clock's enable and the valid bit, set in that order.
    fn vddio2_valid() {
        enable_clock(board::SUPPLY_VDDIO2_RCC_EN_REG, board::SUPPLY_VDDIO2_RCC_EN_MASK);
        set_bits(board::SUPPLY_VDDIO2_VALID_REG, board::SUPPLY_VDDIO2_VALID_MASK);
    }

    fn uart_init(brr: u32) {
        enable_clock(board::VCP_TX_PORT_RCC_EN_REG, board::VCP_TX_PORT_RCC_EN_MASK);
        enable_clock(board::VCP_RX_PORT_RCC_EN_REG, board::VCP_RX_PORT_RCC_EN_MASK);
        enable_clock(board::VCP_RCC_EN_REG, board::VCP_RCC_EN_MASK);

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

    /// A firmware-level abort -- an allocation the heap could not satisfy, or a bug -- says which,
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
    /// layouts: a pending register for each edge, and each line's port selected in EXTI itself.
    const LINES: crate::stm32_pin_events::Lines = crate::stm32_pin_events::Lines {
        mask: instances::EXTI_BASE + chip::exti::IMR1_OFF,
        rising_trigger: instances::EXTI_BASE + chip::exti::RTSR1_OFF,
        falling_trigger: instances::EXTI_BASE + chip::exti::FTSR1_OFF,
        rising_pending: instances::EXTI_BASE + chip::exti::RPR1_OFF,
        falling_pending: instances::EXTI_BASE + chip::exti::FPR1_OFF,
        selectors: instances::EXTI_BASE + chip::exti::EXTICR1_OFF,
        selector_bits: chip::exti::EXTICR1_EXTI1_LSB - chip::exti::EXTICR1_EXTI0_LSB,
        ports: &[
            instances::GPIOA_BASE,
            instances::GPIOB_BASE,
            instances::GPIOC_BASE,
            instances::GPIOD_BASE,
            instances::GPIOE_BASE,
            instances::GPIOF_BASE,
            instances::GPIOG_BASE,
            instances::GPIOH_BASE,
            instances::GPIOI_BASE,
            instances::GPIOJ_BASE,
        ],
        idr: chip::gpio::IDR_OFF,
    };

    /// The vector positions the GPIO lines interrupt at, one a line: EXTI0 to EXTI15 at 11 to 26
    /// (RM0456 Rev 7 Table 186). The memory map gives each the handler below, and the two move
    /// together: a position enabled here with no handler in its slot would fetch a zero vector.
    const EXTI_VECTORS: [u32; 16] = [11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26];

    /// The handler at every GPIO vector (`memory-stm32u5a5.x`): each pending line cleared, then queued
    /// as its pin and the level its pad reads.
    #[unsafe(no_mangle)]
    pub extern "C" fn exti_isr() {
        crate::stm32_pin_events::answer(&LINES);
    }

    const RX_RING: usize = 512;
    static mut RX_BUF: [u8; RX_RING] = [0; RX_RING];
    static RX_HEAD: AtomicUsize = AtomicUsize::new(0);
    static RX_TAIL: AtomicUsize = AtomicUsize::new(0);

    /// The USART1 interrupt handler, named by the vector table (`memory-stm32u5a5.x`, position 61).
    ///
    /// Reading RDR clears RXNE; an overrun is cleared through ICR, the only way this USART clears
    /// one. The byte RDR holds when an overrun is flagged is intact (the shift register's is the one
    /// lost), so it is still taken.
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

    /// The deploy window in the part's flash, bank 2, written through the U5 flash controller.
    ///
    /// The window is a bank of its own, disjoint from the firmware in bank 1, so an erase or a
    /// program does not stall the core at all (RM0456 Rev 7 section 7.3.5), and nothing can erase
    /// the code that is running. Every write and erase is refused, and nothing is erased, on a part
    /// whose banks are not where this map puts them.
    struct FlashDeploy;

    impl FlashDeploy {
        /// The bank-2 pages that hold bytes `offset..end` of the window, or `None` when the part's
        /// banks are not as mapped, `offset` is not on a page boundary, the range is empty, or the
        /// window is not where this map can erase it.
        fn pages(offset: usize, end: usize) -> Option<core::ops::Range<u32>> {
            if !Stm32U5Flash::banks_as_mapped() || offset % PAGE != 0 || end <= offset {
                return None;
            }
            let reach = Stm32U5Flash::window_pages(end);
            let first = reach.start + (offset / PAGE) as u32;
            (first < reach.end).then_some(first..reach.end)
        }

        /// The window's bytes `offset..offset + len`, as the flash reads now.
        fn window(offset: usize, len: usize) -> &'static [u8] {
            unsafe { core::slice::from_raw_parts((stm32u5_flash::image_base() + offset) as *const u8, len) }
        }

        /// Erase `pages`, and say whether every erase was clean and the pages now read erased.
        fn erase_pages(pages: core::ops::Range<u32>, offset: usize) -> bool {
            let len = pages.len() * PAGE;
            let mut flash = Stm32U5Flash;
            if !flash.unlock() {
                return false;
            }
            let mut clean = true;
            for page in pages {
                if !flash.erase_bank2_page(page) {
                    clean = false;
                    break;
                }
            }
            flash.lock();
            clean && Self::window(offset, len).iter().all(|&byte| byte == 0xFF)
        }
    }

    impl FlashSink for FlashDeploy {
        fn image_slice(&self) -> &'static [u8] {
            Self::window(0, stm32u5_flash::image_len())
        }

        fn erase(&mut self) {
            if let Some(first) = Self::pages(0, PAGE) {
                let _ = Self::erase_pages(first, 0);
            }
        }

        fn erase_unit(&self) -> Option<usize> {
            Some(PAGE)
        }

        fn erase_range(&mut self, offset: usize, len: usize) -> bool {
            let in_window = offset.checked_add(len).is_some_and(|end| end <= stm32u5_flash::image_len());
            if !in_window || len % PAGE != 0 {
                return false;
            }
            match Self::pages(offset, offset + len) {
                Some(pages) => Self::erase_pages(pages, offset),
                None => false,
            }
        }

        fn program(&mut self, image: &[u8]) -> bool {
            if image.is_empty() || image.len() > stm32u5_flash::image_len() {
                return false;
            }
            if !Stm32U5Flash::banks_as_mapped() || Stm32U5Flash::window_pages(image.len()).is_empty() {
                return false;
            }
            stm32u5_deploy::write_chunk(&mut PartWindow, 0, image)
        }

        fn program_chunk(&mut self, offset: usize, chunk: &[u8], total: usize) -> bool {
            let Some(end) = offset.checked_add(chunk.len()) else {
                return false;
            };
            if total > stm32u5_flash::image_len() || end > total || offset % QUAD_WORD != 0 {
                return false;
            }
            if !Stm32U5Flash::banks_as_mapped() || Stm32U5Flash::window_pages(total).is_empty() {
                return false;
            }
            stm32u5_deploy::write_chunk(&mut PartWindow, offset, chunk)
        }
    }

    /// The deploy window as [`stm32u5_deploy::write_chunk`] drives it, over the part's own flash
    /// controller: the reads, erases and programs of [`FlashDeploy`], with its refusals.
    struct PartWindow;

    impl stm32u5_deploy::Window for PartWindow {
        fn read(&self, offset: usize, len: usize) -> &[u8] {
            FlashDeploy::window(offset, len)
        }

        fn erase(&mut self, pages: core::ops::Range<usize>) -> bool {
            let offset = pages.start * PAGE;
            match FlashDeploy::pages(offset, pages.end * PAGE) {
                Some(bank_pages) => FlashDeploy::erase_pages(bank_pages, offset),
                None => false,
            }
        }

        fn program(&mut self, offset: usize, bytes: &[u8]) -> bool {
            let mut flash = Stm32U5Flash;
            if !flash.unlock() {
                return false;
            }
            let clean = flash.program_quad_words(stm32u5_flash::image_base() + offset, bytes);
            flash.lock();
            clean
        }
    }

    /// Any exception or fault vector lands here: the stack's limit is lifted, then the fault is
    /// reported and the board resets into serve.
    ///
    /// The limit comes off before anything pushes. A fault the limit raised leaves the stack at its
    /// floor, so a handler whose first push went below it would fault again inside the fault and
    /// lock the core up instead of resetting it. What the report then pushes lands in the top of the
    /// heap, which the reset abandons.
    #[unsafe(no_mangle)]
    #[unsafe(naked)]
    pub extern "C" fn fault() -> ! {
        core::arch::naked_asm!(
            "movs r0, #0",
            "msr MSPLIM, r0",
            "b {report}",
            report = sym report_fault,
        )
    }

    /// Say which fault and where, then reset into serve.
    ///
    /// The status registers are architectural: CFSR says why -- a bus fault, an unaligned access, an
    /// undefined instruction, a stack past its floor -- and the address registers say where when
    /// their valid bits are set. The exception number comes from IPSR, so a fault taken inside an
    /// interrupt is told apart from one on the main path.
    extern "C" fn report_fault() -> ! {
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
        guard_stack();
        let (start, bytes) = heap();
        unsafe {
            ALLOCATOR.init(start, bytes);
        }
        clock_init_16mhz();
        vddio2_valid();
        uart_init(BRR_115200_AT_16MHZ);
        systick_start(SYST_RELOAD_FULL);
        let _ = crate::systick_clock::install(CPU_HZ);
        crate::stm32_pin_events::init(&LINES, &EXTI_VECTORS);

        lamella_runner::set_board_identity(board::BOARD_MODEL, 0, read_reg(DBGMCU_IDCODE));

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
        "stm32u5-csharp-interpreter is a bare-metal target; build for thumbv8m.main-none-eabi with serve,st-nucleo-u5a5zj-q"
    );
}
