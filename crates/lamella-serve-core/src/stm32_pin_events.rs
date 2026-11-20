//! The STM32 firmware half of the pin-change interrupt seam: the external interrupt controller's
//! GPIO vectors, enabled at boot, and the handler that answers them with a token and a level for
//! every GPIO line it finds pending, on an STM32F7 and on an STM32U5 alike.

/// Register access at absolute addresses: the memory map on the part, a register file on the host.
pub(crate) trait Registers {
    fn read32(&mut self, address: u32) -> u32;
    fn write32(&mut self, address: u32, value: u32);
}

/// Where a part keeps what the handler reads and writes, as absolute addresses taken from its
/// family's generated layouts.
pub(crate) struct Lines {
    /// The interrupt mask register: a bit per line, 1 unmasked.
    pub(crate) mask: u32,
    /// The rising and falling trigger enable registers.
    pub(crate) rising_trigger: u32,
    pub(crate) falling_trigger: u32,
    /// The registers a rising and a falling edge are pending in, each write-one-to-clear; the same
    /// register twice on a controller with one pending register.
    pub(crate) rising_pending: u32,
    pub(crate) falling_pending: u32,
    /// The first port selector register, line 0's selector in its lowest field, four lines a
    /// register.
    pub(crate) selectors: u32,
    /// How many bits one line's selector takes.
    pub(crate) selector_bits: u32,
    /// Each GPIO port's base, port A first: a selector's value indexes it.
    pub(crate) ports: &'static [u32],
    /// The input data register's offset from a port's base.
    pub(crate) idr: u32,
}

/// The sixteen GPIO lines. The controller's further lines are internal sources -- the voltage
/// detector, the RTC, USB wakeup -- which nothing here arms, answers or clears.
const GPIO_LINES: u32 = 0xFFFF;

/// The pending GPIO lines whose interrupt is unmasked: what entered the vector.
fn pending(regs: &mut impl Registers, lines: &Lines) -> u32 {
    let mut pending = regs.read32(lines.rising_pending);
    if lines.falling_pending != lines.rising_pending {
        pending |= regs.read32(lines.falling_pending);
    }
    pending & regs.read32(lines.mask) & GPIO_LINES
}

/// Answers the controller's GPIO vectors: for every GPIO line pending with its interrupt unmasked,
/// clears the line's pending bits, then calls `notify` with the pin -- the line's selected port * 16
/// + the line -- and the level the pad reads.
///
/// Every such line's pending bits are cleared before any pad is read. An edge that lands during the
/// read latches again and enters the vector once more, so the last level reported always matches
/// the pad, at the cost of an occasional duplicate; reading first and clearing after would erase
/// that edge and leave the program holding a level the pad no longer has. A line whose selector
/// names a port the part does not have is cleared and not reported: no pin of this part armed it.
pub(crate) fn service(regs: &mut impl Registers, lines: &Lines, notify: &mut impl FnMut(u32, u8)) {
    let pending = pending(regs, lines);
    if pending == 0 {
        return;
    }
    regs.write32(lines.rising_pending, pending);
    if lines.falling_pending != lines.rising_pending {
        regs.write32(lines.falling_pending, pending);
    }
    let field = (1u32 << lines.selector_bits) - 1;
    for line in 0..16u32 {
        if pending & (1 << line) == 0 {
            continue;
        }
        let selector = regs.read32(lines.selectors + (line / 4) * 4);
        let port = (selector >> ((line % 4) * lines.selector_bits)) & field;
        let Some(&base) = lines.ports.get(port as usize) else {
            continue;
        };
        let level = (regs.read32(base + lines.idr) >> line) & 1;
        notify(port * 16 + line, level as u8);
    }
}

/// Masks every GPIO line, clears their triggers, and clears their pending bits, leaving the
/// controller's internal lines as they are.
pub(crate) fn disarm(regs: &mut impl Registers, lines: &Lines) {
    for register in [lines.mask, lines.rising_trigger, lines.falling_trigger] {
        let value = regs.read32(register);
        regs.write32(register, value & !GPIO_LINES);
    }
    regs.write32(lines.rising_pending, GPIO_LINES);
    if lines.falling_pending != lines.rising_pending {
        regs.write32(lines.falling_pending, GPIO_LINES);
    }
}

/// The part's memory map as a [`Registers`].
#[cfg(target_os = "none")]
pub(crate) struct Mmio;

#[cfg(target_os = "none")]
impl Registers for Mmio {
    fn read32(&mut self, address: u32) -> u32 {
        unsafe { core::ptr::read_volatile(address as *const u32) }
    }
    fn write32(&mut self, address: u32, value: u32) {
        unsafe { core::ptr::write_volatile(address as *mut u32, value) };
    }
}

/// The NVIC's first interrupt set-enable register, NVIC_ISER0 at 0xE000E100 on the Cortex-M7 and the
/// Cortex-M33 alike (PM0253 Table 40 and PM0264 Table 49); NVIC_ISERn's bits enable interrupts 32n
/// to 32n + 31, and a written 1 enables its interrupt while a written 0 changes nothing (PM0253
/// Table 42).
#[cfg(target_os = "none")]
const NVIC_ISER0: u32 = 0xE000_E100;

/// Disarms every GPIO line, then enables each of `vectors`, the positions the part's GPIO lines
/// interrupt at. Call once at boot, before a program can arm a line.
#[cfg(target_os = "none")]
pub(crate) fn init(lines: &Lines, vectors: &[u32]) {
    disarm(&mut Mmio, lines);
    for &vector in vectors {
        unsafe { core::ptr::write_volatile((NVIC_ISER0 + (vector / 32) * 4) as *mut u32, 1 << (vector % 32)) };
    }
}

/// Disarms every GPIO line, once a program has ended: nothing drains the queue until the next
/// program starts, so an armed line would only interrupt the serve.
#[cfg(target_os = "none")]
pub(crate) fn disarm_every_line(lines: &Lines) {
    disarm(&mut Mmio, lines);
}

/// The pin-event queue for a fresh `Vm`: every GPIO line disarmed and every queued event dropped
/// first, so the program it runs hears only the pins it arms.
#[cfg(target_os = "none")]
pub(crate) fn source_for_a_new_program(lines: &Lines) -> lamella_cil_runtime::PinEventSource {
    disarm(&mut Mmio, lines);
    let source = crate::pin_events::source();
    while (source.next)().is_some() {}
    let _ = (source.drain_overflowed)();
    source
}

/// Answers a GPIO vector on the part: [`service`], each event queued through the runtime's seam.
#[cfg(target_os = "none")]
pub(crate) fn answer(lines: &Lines) {
    service(&mut Mmio, lines, &mut |token, level| {
        unsafe { crate::pin_events::lamella_isr_notify(token, level) }
    });
}
