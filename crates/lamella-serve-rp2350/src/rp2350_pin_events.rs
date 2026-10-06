//! The RP2350's firmware half of the pin-change interrupt seam: the interrupt IO_BANK0 raises for
//! processor 0, enabled at boot, and the handler that answers it with a token and a level for every
//! pin it finds pending.

#[allow(dead_code)]
#[path = "../../../csp/rp2350/rust/rp2350_io_bank0_layout.rs"]
pub(crate) mod io_bank0;

#[allow(dead_code)]
#[path = "../../../csp/rp2350/rust/rp2350_sio_layout.rs"]
pub(crate) mod sio;

use crate::rp2350_instances as chip;

/// The interrupt registers cover GPIO0 to GPIO47, eight pins a register.
const INT_REGISTERS: u32 = 6;

/// One pin's two edge bits, in its nibble.
const EDGE_BITS: u32 = io_bank0::GPIO_INT_EDGE_LOW | io_bank0::GPIO_INT_EDGE_HIGH;

/// Every pin's edge bits in one interrupt register; the level bits are left out, as nothing here enables
/// them and they cannot be cleared.
const EDGES: u32 = {
    let mut all = 0;
    let mut pin = 0;
    while pin < io_bank0::GPIO_INT_PINS {
        all |= EDGE_BITS << (pin * io_bank0::GPIO_INT_BITS);
        pin += 1;
    }
    all
};

/// Register access at absolute addresses: the memory map on the part, a register file on the host.
pub(crate) trait Registers {
    fn read32(&mut self, address: u32) -> u32;
    fn write32(&mut self, address: u32, value: u32);
}

fn register(base_offset: u32, index: u32) -> u32 {
    chip::IO_BANK0_BASE + base_offset + index * io_bank0::GPIO_INT_STRIDE
}

/// Answers IO_IRQ_BANK0: for every pin with an edge both pending and enabled for processor 0, clears that
/// pin's pending edges, then calls `notify` with the pin and the level its pad reads.
///
/// The edges are cleared before any pad is read. An edge that lands during the read latches again and
/// enters this vector once more, so the last level reported always matches the pad, at the cost of an
/// occasional duplicate; reading first and clearing after would erase that edge and leave the program
/// holding a level the pad no longer has.
pub(crate) fn service(regs: &mut impl Registers, notify: &mut impl FnMut(u32, u8)) {
    for index in 0..INT_REGISTERS {
        let pending = regs.read32(register(io_bank0::PROC0_INTS0_OFF, index)) & EDGES;
        if pending == 0 {
            continue;
        }
        regs.write32(register(io_bank0::INTR0_OFF, index), pending);
        let first = index * io_bank0::GPIO_INT_PINS;
        let input = if first < 32 { sio::GPIO_IN_OFF } else { sio::GPIO_HI_IN_OFF };
        let levels = regs.read32(chip::SIO_BASE + input);
        for nibble in 0..io_bank0::GPIO_INT_PINS {
            if (pending >> (nibble * io_bank0::GPIO_INT_BITS)) & EDGE_BITS != 0 {
                let pin = first + nibble;
                notify(pin, u8::from((levels >> (pin % 32)) & 1 != 0));
            }
        }
    }
}

/// Disables every pin's interrupt to processor 0 and clears every latched edge.
pub(crate) fn disarm(regs: &mut impl Registers) {
    for index in 0..INT_REGISTERS {
        regs.write32(register(io_bank0::PROC0_INTE0_OFF, index), 0);
        regs.write32(register(io_bank0::INTR0_OFF, index), EDGES);
    }
}

/// The Cortex-M33 NVIC's first interrupt set-enable register.
#[cfg(target_os = "none")]
const NVIC_ISER0: u32 = 0xE000_E100;

/// IO_IRQ_BANK0, the bank's interrupt to processor 0 (RP2350 datasheet 3.2, Table 95).
#[cfg(target_os = "none")]
const IO_IRQ_BANK0: u32 = 21;

/// The part's memory map as a [`Registers`].
#[cfg(target_os = "none")]
struct Mmio;

#[cfg(target_os = "none")]
impl Registers for Mmio {
    fn read32(&mut self, address: u32) -> u32 {
        unsafe { core::ptr::read_volatile(address as *const u32) }
    }
    fn write32(&mut self, address: u32, value: u32) {
        unsafe { core::ptr::write_volatile(address as *mut u32, value) };
    }
}

/// Disarms every pin, then enables IO_IRQ_BANK0. Call once at boot, before a program can arm a pin.
#[cfg(target_os = "none")]
pub(crate) fn init() {
    disarm(&mut Mmio);
    unsafe { core::ptr::write_volatile(NVIC_ISER0 as *mut u32, 1 << IO_IRQ_BANK0) };
}

/// Disarms every pin, once a program has ended: nothing drains the queue until the next program
/// starts, so an armed pin would only interrupt the serve.
#[cfg(target_os = "none")]
pub(crate) fn disarm_every_pin() {
    disarm(&mut Mmio);
}

/// The pin-event queue for a fresh `Vm`: every pin disarmed and every queued event dropped first, so the
/// program it runs hears only the pins it arms.
#[cfg(target_os = "none")]
pub(crate) fn source_for_a_new_program() -> lamella_cil_runtime::PinEventSource {
    disarm(&mut Mmio);
    let source = crate::pin_events::source();
    while (source.next)().is_some() {}
    let _ = (source.drain_overflowed)();
    source
}

/// IO_IRQ_BANK0's handler, at the slot `memory-rp2350.x` gives IRQ 21.
#[cfg(target_os = "none")]
#[unsafe(no_mangle)]
pub extern "C" fn io_bank0_isr() {
    service(&mut Mmio, &mut |token, level| {
        unsafe { crate::pin_events::lamella_isr_notify(token, level) }
    });
}
