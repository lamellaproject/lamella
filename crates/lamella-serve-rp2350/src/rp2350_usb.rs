//! The RP2350 chip half of the native-USB Lamella Link carrier -- a
//! hand-rolled `usb_device::bus::UsbBus` over the RP2350's USB device controller, plus the
//! 48 MHz clock tree it needs and the chip-unique serial the shared identity advertises.
//!
//! Facts from the RP2350 datasheet:
//! - USB (12.7): controller registers at `0x5011_0000`, 4 kB dual-port SRAM at `0x5010_0000`
//!   holding the SETUP packet (offset 0), per-endpoint control + buffer-control words, and the
//!   data buffers. EP0 has no endpoint-control word; its buffer is fixed at `0x100` and its
//!   completion reporting is enabled by `SIE_CTRL.EP0_INT_1BUF`. Buffer ownership is the
//!   buffer-control `AVAILABLE` bit; because clk_sys and clk_usb differ, the rest of the word is
//!   written first and `AVAILABLE` is set in a second write after >= 1 clk_usb cycle (12.7.3.7.1).
//! - Clocks (8.1, 8.6): the controller needs clk_usb = 48 MHz (XOSC 12 MHz -> PLL_USB: FBDIV 100,
//!   VCO 1200 MHz, postdiv 5 x 5), and clk_sys must run at least 10 % faster than clk_usb while
//!   the peripheral is in use (erratum RP2350-E12) -- so this also brings clk_sys to the rated
//!   150 MHz (PLL_SYS: FBDIV 125, VCO 1500 MHz, postdiv 5 x 2).
//! - Identity (13.9, OTP): CHIPID0..3 (OTP rows 0x000-0x003) hold a 64-bit per-device identifier,
//!   read ECC-corrected through the `OTP_DATA` alias -- the USB iSerialNumber, so a host picker
//!   can tell boards apart.
//!
//! The driver is polled (`UsbBus::poll` from the firmware's main loop; no USB interrupt) and single
//! buffered: 64-byte bulk packets are what the Lamella Link carrier chunks to, and the main loop's
//! poll cadence keeps up with a Full Speed host without double buffering.

use usb_device::bus::UsbBusAllocator;
use usb_device::endpoint::{EndpointAddress, EndpointType};
use usb_device::{UsbDirection, UsbError};

fn write_reg(addr: usize, val: u32) {
    unsafe { core::ptr::write_volatile(addr as *mut u32, val) };
}
fn read_reg(addr: usize) -> u32 {
    unsafe { core::ptr::read_volatile(addr as *const u32) }
}

// ---- clock tree ------------------------------------------------------------------------------
// The sequence is rp2350_usb_clocks.rs, written over a register trait so a host test can run it
// over a model of the clock generators; this carrier supplies the chip's own registers.

#[path = "rp2350_usb_clocks.rs"]
mod clocks;

pub use clocks::CLK_SYS_HZ;

/// The chip's own registers, for the clock sequence.
struct Mmio;

impl clocks::Registers for Mmio {
    fn read(&mut self, address: usize) -> u32 {
        read_reg(address)
    }
    fn write(&mut self, address: usize, value: u32) {
        write_reg(address, value);
    }
}

// USBCTRL (28) is hand-coded because the chip's instance rows do not include the USB controller.
const RESET_USBCTRL: u32 = 1 << 28;

/// Pulse a RESETS bit with both edges confirmed, on the chip's registers.
fn reset_cycle(reset_bit: u32) {
    clocks::reset_cycle(&mut Mmio, reset_bit);
}

/// Bring the USB clock tree up: clk_sys at 150 MHz from PLL_SYS, clk_usb at 48 MHz from PLL_USB,
/// both from the crystal ([`clocks::init`], which says how each switch is made). Returns `false`
/// if the crystal, a switch or a PLL never comes ready, so the firmware degrades to UART-only
/// operation rather than waiting forever.
pub fn clocks_init() -> bool {
    clocks::init(&mut Mmio)
}

// ---- chip-unique serial ------------------------------------------------------------------------

/// The ECC read alias of the OTP: a 32-bit read returns two neighboring 16-bit rows. Rows
/// 0x000-0x003 are CHIPID0..3, the 64-bit per-device identifier.
const OTP_DATA_BASE: usize = 0x4013_0000;

/// The board's unique serial-number string: the 64-bit OTP chip id as 16 uppercase hex digits
/// (device-id high word first). Stable across boots and unique per unit, so a host picker can
/// address one of several attached Lamella Link boards. Call once at boot, before `build_device`.
pub fn serial_number() -> &'static str {
    static mut SERIAL: [u8; 16] = [0; 16];
    let low = read_reg(OTP_DATA_BASE);
    let high = read_reg(OTP_DATA_BASE + 4);
    let mut id = (u64::from(high) << 32) | u64::from(low);
    unsafe {
        let buf = &mut *core::ptr::addr_of_mut!(SERIAL);
        let mut i = 16;
        while i > 0 {
            i -= 1;
            let nibble = (id & 0xf) as usize;
            buf[i] = b"0123456789ABCDEF"[nibble];
            id >>= 4;
        }
        core::str::from_utf8_unchecked(&*core::ptr::addr_of!(SERIAL))
    }
}

// ---- the device controller ---------------------------------------------------------------------

const DPRAM: usize = 0x5010_0000;
const REGS: usize = 0x5011_0000;
const REGS_SET: usize = REGS + 0x2000;
const REGS_CLR: usize = REGS + 0x3000;

const ADDR_ENDP: usize = REGS + 0x00;
const MAIN_CTRL: usize = REGS + 0x40;
const SIE_CTRL: usize = REGS + 0x4c;
const SIE_STATUS: usize = REGS + 0x50;
const BUFF_STATUS: usize = REGS + 0x58;
const EP_STALL_ARM: usize = REGS + 0x68;
const USB_MUXING: usize = REGS + 0x74;
const USB_PWR: usize = REGS + 0x78;

const MAIN_CTRL_CONTROLLER_EN: u32 = 1 << 0; // written alone it also clears PHY_ISO (bit 2)
const SIE_CTRL_EP0_INT_1BUF: u32 = 1 << 29; // EP0 completions appear in BUFF_STATUS
const SIE_CTRL_PULLUP_EN: u32 = 1 << 16; // present as a Full Speed device
const SIE_STATUS_DATA_SEQ_ERROR: u32 = 1 << 31;
const SIE_STATUS_ACK_REC: u32 = 1 << 30;
const SIE_STATUS_RX_TIMEOUT: u32 = 1 << 27;
const SIE_STATUS_RX_OVERFLOW: u32 = 1 << 26;
const SIE_STATUS_BIT_STUFF_ERROR: u32 = 1 << 25;
const SIE_STATUS_CRC_ERROR: u32 = 1 << 24;
const SIE_STATUS_BUS_RESET: u32 = 1 << 19;
const SIE_STATUS_TRANS_COMPLETE: u32 = 1 << 18;
const SIE_STATUS_SETUP_REC: u32 = 1 << 17;
const SIE_STATUS_RX_SHORT_PACKET: u32 = 1 << 12;
const USB_MUXING_TO_PHY: u32 = 1 << 0;
const USB_MUXING_SOFTCON: u32 = 1 << 3;
const USB_PWR_VBUS_DETECT: u32 = 1 << 2;
const USB_PWR_VBUS_DETECT_OVERRIDE_EN: u32 = 1 << 3;

// Endpoint-control word (DPRAM, endpoints 1-15): enable + completion reporting + type + the
// 64-byte-aligned data-buffer offset in bits 15:6 (written as the plain byte offset).
const EP_CTRL_ENABLE: u32 = 1 << 31;
const EP_CTRL_INT_PER_BUFFER: u32 = 1 << 29;
const EP_CTRL_TYPE_LSB: u32 = 26;

// Buffer-control word, single-buffered half (bits 15:0).
const BUF_CTRL_FULL: u32 = 1 << 15;
const BUF_CTRL_LAST: u32 = 1 << 14;
const BUF_CTRL_DATA1: u32 = 1 << 13;
const BUF_CTRL_STALL: u32 = 1 << 11;
const BUF_CTRL_AVAILABLE: u32 = 1 << 10;
const BUF_CTRL_LEN_MASK: u32 = 0x3ff;

/// Endpoints this driver supports per direction: EP0 (control) + the Lamella Link bulk pair leaves
/// two spares.
const MAX_ENDPOINTS: usize = 4;

/// The first DPRAM byte available for allocated data buffers: after the control words (0x0-0xff)
/// and the fixed EP0 buffer (0x100-0x17f; the optional second EP0 buffer is unused -- EP0 is
/// single buffered here).
const FIRST_BUFFER_OFFSET: u16 = 0x180;
const DPRAM_BYTES: u16 = 4096;

/// The endpoint-control word address for endpoint `index` (1-15) in `dir`. EP0 has none.
fn ep_ctrl_addr(dir: UsbDirection, index: usize) -> usize {
    let base = match dir {
        UsbDirection::In => DPRAM + 0x08,
        UsbDirection::Out => DPRAM + 0x0c,
    };
    base + (index - 1) * 8
}

/// The buffer-control word address for endpoint `index` in `dir`.
fn buf_ctrl_addr(dir: UsbDirection, index: usize) -> usize {
    let base = match dir {
        UsbDirection::In => DPRAM + 0x80,
        UsbDirection::Out => DPRAM + 0x84,
    };
    base + index * 8
}

/// At least one clk_usb (48 MHz) cycle at any clk_sys this firmware runs, so a two-step
/// buffer-control write is safely ordered across the clock-domain boundary (12.7.3.7.1).
fn settle() {
    for _ in 0..8 {
        unsafe { core::arch::asm!("nop", options(nomem, nostack, preserves_flags)) };
    }
}

/// Write a buffer-control word in the documented two steps: everything except AVAILABLE first,
/// then AVAILABLE once the rest is stable in the controller's clock domain.
fn arm_buffer(addr: usize, value_with_available: u32) {
    write_reg(addr, value_with_available & !BUF_CTRL_AVAILABLE);
    settle();
    write_reg(addr, value_with_available);
}

#[derive(Clone, Copy, Default)]
struct EndpointSlot {
    allocated: bool,
    ep_type: u8, // the controller's 2-bit type encoding
    max_packet: u16,
    buffer: u16, // DPRAM byte offset of the data buffer (EP0: 0x100)
}

/// The RP2350 USB device controller as a `usb_device` bus.
///
/// Mutability: `usb-device` shares the bus by `&self` after `enable`, requiring `Sync`. This
/// firmware is single-core and polls the bus from one loop with no USB interrupt handler, so
/// the interior mutability is plain `Cell`s and the `Sync` promise is upheld by construction
/// (nothing else can observe the cells).
pub struct UsbBus {
    in_ep: [EndpointSlot; MAX_ENDPOINTS],
    out_ep: [EndpointSlot; MAX_ENDPOINTS],
    next_buffer: u16,
    /// Bit n set = the next packet armed on EPn (per direction) uses DATA1.
    next_pid_in: core::cell::Cell<u16>,
    next_pid_out: core::cell::Cell<u16>,
    /// Level-latched "an OUT packet waits in EPn's buffer" -- held until `read` consumes it.
    out_pending: core::cell::Cell<u16>,
    /// Level-latched "a SETUP packet waits at DPRAM 0" -- held until `read` on EP0 consumes it.
    setup_pending: core::cell::Cell<bool>,
}

unsafe impl Sync for UsbBus {}

impl UsbBus {
    /// The controller wrapped for `usb-device`. Call once, after [`clocks_init`] returned
    /// `true`; `UsbDevice::build` then enables the controller and connects to the bus.
    pub fn new() -> UsbBusAllocator<UsbBus> {
        UsbBusAllocator::new(UsbBus {
            in_ep: [EndpointSlot::default(); MAX_ENDPOINTS],
            out_ep: [EndpointSlot::default(); MAX_ENDPOINTS],
            next_buffer: FIRST_BUFFER_OFFSET,
            next_pid_in: core::cell::Cell::new(0),
            next_pid_out: core::cell::Cell::new(0),
            out_pending: core::cell::Cell::new(0),
            setup_pending: core::cell::Cell::new(false),
        })
    }

    fn slot(&self, ep: EndpointAddress) -> Option<&EndpointSlot> {
        let table = match ep.direction() {
            UsbDirection::In => &self.in_ep,
            UsbDirection::Out => &self.out_ep,
        };
        table.get(ep.index()).filter(|slot| slot.allocated)
    }

    /// Use-and-flip the next-arm data PID for `ep`.
    fn take_pid(&self, ep: EndpointAddress) -> u32 {
        let cell = match ep.direction() {
            UsbDirection::In => &self.next_pid_in,
            UsbDirection::Out => &self.next_pid_out,
        };
        let bit = 1u16 << ep.index();
        let mask = cell.get();
        cell.set(mask ^ bit);
        if mask & bit != 0 { BUF_CTRL_DATA1 } else { 0 }
    }

    fn set_pid(&self, ep: EndpointAddress, data1: bool) {
        let cell = match ep.direction() {
            UsbDirection::In => &self.next_pid_in,
            UsbDirection::Out => &self.next_pid_out,
        };
        let bit = 1u16 << ep.index();
        cell.set(if data1 { cell.get() | bit } else { cell.get() & !bit });
    }

    /// Arm OUT endpoint `index` to receive one packet (capacity = its max packet size).
    fn arm_out(&self, index: usize) {
        let slot = &self.out_ep[index];
        let pid = self.take_pid(EndpointAddress::from_parts(index, UsbDirection::Out));
        arm_buffer(
            buf_ctrl_addr(UsbDirection::Out, index),
            u32::from(slot.max_packet) | pid | BUF_CTRL_AVAILABLE,
        );
    }

    /// Program the endpoint-control words and initial buffer states for every allocated
    /// endpoint -- the shared body of `enable` (first bring-up) and `reset` (host bus reset).
    fn configure_endpoints(&self) {
        self.next_pid_in.set(0);
        self.next_pid_out.set(0);
        self.out_pending.set(0);
        self.setup_pending.set(false);
        // EP0 has no endpoint-control word; quiesce both its buffer halves.
        write_reg(buf_ctrl_addr(UsbDirection::In, 0), 0);
        write_reg(buf_ctrl_addr(UsbDirection::Out, 0), 0);
        for index in 1..MAX_ENDPOINTS {
            if self.in_ep[index].allocated {
                let slot = &self.in_ep[index];
                write_reg(
                    ep_ctrl_addr(UsbDirection::In, index),
                    EP_CTRL_ENABLE
                        | EP_CTRL_INT_PER_BUFFER
                        | (u32::from(slot.ep_type) << EP_CTRL_TYPE_LSB)
                        | u32::from(slot.buffer),
                );
                write_reg(buf_ctrl_addr(UsbDirection::In, index), 0);
            }
            if self.out_ep[index].allocated {
                let slot = &self.out_ep[index];
                write_reg(
                    ep_ctrl_addr(UsbDirection::Out, index),
                    EP_CTRL_ENABLE
                        | EP_CTRL_INT_PER_BUFFER
                        | (u32::from(slot.ep_type) << EP_CTRL_TYPE_LSB)
                        | u32::from(slot.buffer),
                );
                self.arm_out(index); // ready for the host's first DATA0
            }
        }
    }
}

/// Bring-up telemetry, readable over SWD without halting the core: a ring of the last 32 EP0-level
/// events, so a failed enumeration shows the exact transfer where the exchange stopped. Each event
/// is one u32: tag<<24 | arg. Tags: 1 bus reset, 2 SETUP latched, 3 SETUP consumed, 4 EP0-IN armed(len),
/// 5 EP0-IN complete, 6 EP0-OUT complete(len), 7 address set, 8 EP0 stall set(dir),
/// 9 EP0-IN write refused (busy).
#[unsafe(no_mangle)]
static mut USB_EP0_RING: [u32; 33] = [0; 33]; // [0] = next slot index, [1..] = events

fn trace(tag: u32, arg: u32) {
    unsafe {
        let ring = core::ptr::addr_of_mut!(USB_EP0_RING).cast::<u32>();
        let index = core::ptr::read_volatile(ring);
        core::ptr::write_volatile(ring.add(1 + (index as usize & 31)), (tag << 24) | (arg & 0x00ff_ffff));
        core::ptr::write_volatile(ring, index.wrapping_add(1));
    }
}

/// Copy `len` bytes between a DPRAM buffer and RAM, bytewise (the DPRAM supports 8-bit access).
fn dpram_write(offset: u16, data: &[u8]) {
    let base = (DPRAM + offset as usize) as *mut u8;
    for (i, byte) in data.iter().enumerate() {
        unsafe { core::ptr::write_volatile(base.add(i), *byte) };
    }
}
fn dpram_read(offset: u16, out: &mut [u8]) {
    let base = (DPRAM + offset as usize) as *const u8;
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = unsafe { core::ptr::read_volatile(base.add(i)) };
    }
}

impl usb_device::bus::UsbBus for UsbBus {
    fn alloc_ep(
        &mut self,
        ep_dir: UsbDirection,
        ep_addr: Option<EndpointAddress>,
        ep_type: EndpointType,
        max_packet_size: u16,
        _interval: u8,
    ) -> usb_device::Result<EndpointAddress> {
        // The controller's 2-bit endpoint type (12.7.3.7.3).
        let type_bits = match ep_type {
            EndpointType::Control => 0u8,
            EndpointType::Isochronous { .. } => 1,
            EndpointType::Bulk => 2,
            EndpointType::Interrupt => 3,
        };
        // EP0 is the only control endpoint the controller models (no control word, fixed buffer).
        let index = match (ep_type, ep_addr) {
            (EndpointType::Control, _) => 0,
            (_, Some(addr)) => addr.index(),
            (_, None) => {
                let table = if ep_dir == UsbDirection::In { &self.in_ep } else { &self.out_ep };
                match (1..MAX_ENDPOINTS).find(|&i| !table[i].allocated) {
                    Some(i) => i,
                    None => return Err(UsbError::EndpointOverflow),
                }
            }
        };
        if index >= MAX_ENDPOINTS || max_packet_size > 64 {
            return Err(UsbError::EndpointOverflow);
        }
        let slot = match ep_dir {
            UsbDirection::In => &mut self.in_ep[index],
            UsbDirection::Out => &mut self.out_ep[index],
        };
        if slot.allocated {
            return Err(UsbError::InvalidEndpoint);
        }
        let buffer = if index == 0 {
            0x100 // hardware-fixed EP0 buffer, shared by both directions
        } else {
            if self.next_buffer + 64 > DPRAM_BYTES {
                return Err(UsbError::EndpointOverflow);
            }
            let offset = self.next_buffer;
            self.next_buffer += 64;
            offset
        };
        *slot = EndpointSlot { allocated: true, ep_type: type_bits, max_packet: max_packet_size, buffer };
        Ok(EndpointAddress::from_parts(index, ep_dir))
    }

    fn enable(&mut self) {
        // Reset-cycle the controller (both edges confirmed -- see reset_cycle); the DPRAM is
        // only accessible out of reset.
        reset_cycle(RESET_USBCTRL);
        let mut word = 0;
        while word < DPRAM_BYTES as usize {
            write_reg(DPRAM + word, 0);
            word += 4;
        }
        // Controller to the on-chip PHY; VBUS forced detected (the Pico 2 has no VBUS sense
        // wired to the controller); device mode, PHY isolation cleared, controller on.
        write_reg(USB_MUXING, USB_MUXING_TO_PHY | USB_MUXING_SOFTCON);
        write_reg(USB_PWR, USB_PWR_VBUS_DETECT | USB_PWR_VBUS_DETECT_OVERRIDE_EN);
        write_reg(MAIN_CTRL, MAIN_CTRL_CONTROLLER_EN);
        // EP0 completions report in BUFF_STATUS (EP0 has no endpoint-control word to enable
        // them per endpoint).
        write_reg(SIE_CTRL, SIE_CTRL_EP0_INT_1BUF);
        self.configure_endpoints();
        // Present to the host: the DP pull-up announces a Full Speed device.
        write_reg(SIE_CTRL + (REGS_SET - REGS), SIE_CTRL_PULLUP_EN);
    }

    fn reset(&self) {
        write_reg(SIE_STATUS + (REGS_CLR - REGS), SIE_STATUS_BUS_RESET);
        write_reg(ADDR_ENDP, 0);
        write_reg(EP_STALL_ARM, 0);
        self.configure_endpoints();
    }

    fn set_device_address(&self, addr: u8) {
        // usb-device calls this after the SET_ADDRESS status stage completed, which is when the
        // controller must start answering on the new address (12.7: ADDR_ENDP.ADDRESS).
        trace(7, u32::from(addr));
        write_reg(ADDR_ENDP, u32::from(addr));
    }

    fn write(&self, ep_addr: EndpointAddress, buf: &[u8]) -> usb_device::Result<usize> {
        if ep_addr.direction() != UsbDirection::In {
            return Err(UsbError::InvalidEndpoint);
        }
        let slot = self.slot(ep_addr).ok_or(UsbError::InvalidEndpoint)?;
        if buf.len() > usize::from(slot.max_packet) {
            return Err(UsbError::BufferOverflow);
        }
        let ctrl = buf_ctrl_addr(UsbDirection::In, ep_addr.index());
        if read_reg(ctrl) & BUF_CTRL_AVAILABLE != 0 {
            if ep_addr.index() == 0 {
                trace(9, 0);
            }
            return Err(UsbError::WouldBlock); // the previous packet has not been taken yet
        }
        dpram_write(slot.buffer, buf);
        let pid = self.take_pid(ep_addr);
        if ep_addr.index() == 0 {
            trace(4, buf.len() as u32 | (pid >> 3)); // bit 10 = DATA1
        }
        arm_buffer(
            ctrl,
            buf.len() as u32 | pid | BUF_CTRL_FULL | BUF_CTRL_LAST | BUF_CTRL_AVAILABLE,
        );
        Ok(buf.len())
    }

    fn read(&self, ep_addr: EndpointAddress, buf: &mut [u8]) -> usb_device::Result<usize> {
        if ep_addr.direction() != UsbDirection::Out {
            return Err(UsbError::InvalidEndpoint);
        }
        let index = ep_addr.index();
        self.slot(ep_addr).ok_or(UsbError::InvalidEndpoint)?;

        // A pending SETUP takes precedence on EP0: it lives in the dedicated DPRAM area (not the
        // EP0 buffer), begins a fresh control transfer (both EP0 PIDs restart at DATA1), and
        // cancels whatever the aborted previous transfer left armed.
        if index == 0 && self.setup_pending.get() {
            if buf.len() < 8 {
                return Err(UsbError::BufferOverflow);
            }
            dpram_read(0, &mut buf[..8]);
            trace(3, u32::from(buf[0]) | (u32::from(buf[1]) << 8) | (u32::from(buf[3]) << 16));
            self.setup_pending.set(false);
            self.out_pending.set(self.out_pending.get() & !1); // stale pre-SETUP data
            write_reg(buf_ctrl_addr(UsbDirection::In, 0), 0); // disarm an unsent IN reply
            self.set_pid(EndpointAddress::from_parts(0, UsbDirection::In), true);
            self.set_pid(EndpointAddress::from_parts(0, UsbDirection::Out), true);
            self.arm_out(0); // ready for a DATA1 data stage or the status ZLP
            return Ok(8);
        }

        let bit = 1u16 << index;
        if self.out_pending.get() & bit == 0 {
            return Err(UsbError::WouldBlock);
        }
        let ctrl = buf_ctrl_addr(UsbDirection::Out, index);
        let len = (read_reg(ctrl) & BUF_CTRL_LEN_MASK) as usize;
        if len > buf.len() {
            // Class-level bug (buffer smaller than the negotiated max packet): drop the packet
            // rather than wedging the endpoint.
            self.out_pending.set(self.out_pending.get() & !bit);
            self.arm_out(index);
            return Err(UsbError::BufferOverflow);
        }
        let slot = self.slot(ep_addr).ok_or(UsbError::InvalidEndpoint)?;
        dpram_read(slot.buffer, &mut buf[..len]);
        self.out_pending.set(self.out_pending.get() & !bit);
        self.arm_out(index);
        Ok(len)
    }

    fn set_stalled(&self, ep_addr: EndpointAddress, stalled: bool) {
        let index = ep_addr.index();
        if self.slot(ep_addr).is_none() {
            return;
        }
        let ctrl = buf_ctrl_addr(ep_addr.direction(), index);
        if stalled {
            if index == 0 {
                trace(8, if ep_addr.direction() == UsbDirection::In { 1 } else { 0 });
                // EP0 stalls are armed via EP_STALL_ARM (the controller clears the arm on the
                // next SETUP, as the USB spec requires).
                let arm_bit = if ep_addr.direction() == UsbDirection::In { 1 } else { 2 };
                write_reg(EP_STALL_ARM + (REGS_SET - REGS), arm_bit);
            }
            write_reg(ctrl, BUF_CTRL_STALL);
        } else {
            // usb-device's control pipe unstalls EP0-OUT habitually (after every SETUP parse
            // and before every status stage) -- when no STALL is actually set this must leave
            // the buffer state alone, or it disarms the very buffer waiting for the status
            // ZLP and the host NAK-retries the status stage into a timeout.
            if read_reg(ctrl) & BUF_CTRL_STALL == 0 {
                return;
            }
            write_reg(ctrl, 0);
            if index != 0 {
                // Halt cleared: the data toggle restarts at DATA0 (USB 2.0, 9.4.5).
                self.set_pid(ep_addr, false);
            }
            if ep_addr.direction() == UsbDirection::Out {
                // Prepared to receive again (the trait's contract for an OUT unstall).
                self.out_pending.set(self.out_pending.get() & !(1u16 << index));
                self.arm_out(index);
            }
        }
    }

    fn is_stalled(&self, ep_addr: EndpointAddress) -> bool {
        read_reg(buf_ctrl_addr(ep_addr.direction(), ep_addr.index())) & BUF_CTRL_STALL != 0
    }

    fn suspend(&self) {
        // Bus-powered dev board: nothing to power down. The main loop keeps polling and the
        // controller resumes with the host traffic.
    }

    fn resume(&self) {}

    fn poll(&self) -> usb_device::bus::PollResult {
        // Bring-up telemetry, readable over SWD without halting the core: how often the main loop
        // actually services the controller -- a stalled pump shows as a frozen count while the
        // bus is active.
        #[unsafe(no_mangle)]
        static mut RP2350_USB_POLLS: u32 = 0;
        unsafe {
            let polls = core::ptr::addr_of_mut!(RP2350_USB_POLLS);
            core::ptr::write_volatile(polls, core::ptr::read_volatile(polls).wrapping_add(1));
        }

        let sie = read_reg(SIE_STATUS);
        if sie & SIE_STATUS_BUS_RESET != 0 {
            // Reported level-style; `reset` clears the bit after usb-device reacts.
            trace(1, 0);
            return usb_device::bus::PollResult::Reset;
        }
        if sie & SIE_STATUS_SETUP_REC != 0 {
            write_reg(SIE_STATUS + (REGS_CLR - REGS), SIE_STATUS_SETUP_REC);
            self.setup_pending.set(true);
            trace(2, read_reg(DPRAM) & 0xffff); // bmRequestType | bRequest
        }
        // Keep the write-clear noise bits from accumulating (they are status-only here; the
        // controller retries per the USB protocol).
        let noise = sie
            & (SIE_STATUS_DATA_SEQ_ERROR
                | SIE_STATUS_ACK_REC
                | SIE_STATUS_RX_TIMEOUT
                | SIE_STATUS_RX_OVERFLOW
                | SIE_STATUS_BIT_STUFF_ERROR
                | SIE_STATUS_CRC_ERROR
                | SIE_STATUS_RX_SHORT_PACKET
                | SIE_STATUS_TRANS_COMPLETE);
        if noise != 0 {
            write_reg(SIE_STATUS + (REGS_CLR - REGS), noise);
        }

        // Harvest completions: IN completions report once (edge); OUT arrivals latch into
        // out_pending until read() consumes them (level, per the PollResult contract).
        let buff = read_reg(BUFF_STATUS);
        let mut ep_in_complete: u16 = 0;
        if buff != 0 {
            write_reg(BUFF_STATUS + (REGS_CLR - REGS), buff);
            let mut out_now = self.out_pending.get();
            for index in 0..MAX_ENDPOINTS {
                if buff & (1 << (2 * index)) != 0 {
                    ep_in_complete |= 1 << index;
                    if index == 0 {
                        trace(5, 0);
                    }
                }
                if buff & (1 << (2 * index + 1)) != 0 {
                    out_now |= 1 << index;
                    if index == 0 {
                        trace(6, read_reg(buf_ctrl_addr(UsbDirection::Out, 0)) & BUF_CTRL_LEN_MASK);
                    }
                }
            }
            self.out_pending.set(out_now);
        }

        let ep_out = self.out_pending.get();
        let ep_setup = u16::from(self.setup_pending.get());
        if ep_out != 0 || ep_in_complete != 0 || ep_setup != 0 {
            return usb_device::bus::PollResult::Data { ep_out, ep_in_complete, ep_setup };
        }
        usb_device::bus::PollResult::None
    }
}
