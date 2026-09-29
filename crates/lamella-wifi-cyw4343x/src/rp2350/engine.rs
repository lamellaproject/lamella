//! The engine: the bring-up of the pins and the state machine, the frame
//! as programmed I/O through the FIFOs, the strap, the power line and the
//! interrupt at the IO bank.

#![allow(unsafe_code)]

use core::ptr::{read_volatile, write_volatile};

use super::{
    ATOMIC_CLR, ATOMIC_SET, ATOMIC_XOR, CTRL_SM0_CLKDIV_RESTART, CTRL_SM0_ENABLE, CTRL_SM0_RESTART,
    FDEBUG_RXSTALL0, FDEBUG_SM0_ALL, FSTAT_RXEMPTY0, FSTAT_TXEMPTY0, FSTAT_TXFULL0, FUNCSEL_PIO0,
    FUNCSEL_SIO, GPIO24_LEVEL_HIGH, INSTR_DATA_INPUT, INSTR_PARK, IO_BANK0, Ladder, OEOVER_ENABLE,
    PAD_CLOCK_LINE, PAD_CONTROL_LINE, PAD_DATA_LINE, PAD_ISO, PHASE_DEFAULT, PHASE_FIRST,
    PHASE_LAST, PIN_CLOCK, PIN_DATA, PIN_POWER, PIN_SELECT, PIO_CTRL, PIO_FDEBUG, PIO_FSTAT,
    PIO_INPUT_SYNC_BYPASS, PIO_INSTR_MEM0, PIO_RXF0, PIO_SM0_ADDR, PIO_SM0_CLKDIV,
    PIO_SM0_EXECCTRL, PIO_SM0_INSTR, PIO_SM0_PINCTRL, PIO_SM0_SHIFTCTRL, PIO_TXF0, PIO0,
    PROC0_INTE3, RESET, RESET_DONE, RESET_IO_BANK0, RESET_PADS_BANK0, RESET_PIO0, RESETS,
    SHIFTCTRL_FJOIN_RX, SIO, SIO_GPIO_IN, SIO_GPIO_OE_CLR, SIO_GPIO_OE_SET, SIO_GPIO_OUT_CLR,
    SIO_GPIO_OUT_SET, clkdiv_word, execctrl_word, gpio_ctrl, header_x, header_y, ladder, pad,
    pinctrl_word, program, setting_of, shiftctrl_word,
};
use crate::error::Refusal;
use crate::gspi::{GspiWire, Setting};

/// The stage name of a reset the controller never reported done.
pub const STAGE_RESET: &str = "gSPI engine: reset";
/// The stage name of a frame asked of an engine that was never brought up.
pub const STAGE_BRING_UP: &str = "gSPI engine: bring-up";
/// The stage name of a transmit FIFO that never drained.
pub const STAGE_TX_FIFO: &str = "gSPI engine: transmit FIFO";
/// The stage name of a response word that never came.
pub const STAGE_RX_FIFO: &str = "gSPI engine: receive FIFO";
/// The stage name of a machine that did not park after the frame.
pub const STAGE_PARK: &str = "gSPI engine: park";
/// The stage name of response words left in the FIFO after the frame.
pub const STAGE_RESPONSE_LEFT: &str = "gSPI engine: response left";

/// The bound on a wait for a FIFO flag or the park, in polls of the status
/// register: the host clocks the bus, so a machine that does not move is a
/// configuration fault, never the chip's doing.
const FIFO_SPIN: u32 = 1 << 20;
/// The bound on the reset controller's done flag.
const RESET_SPIN: u32 = 1 << 20;

/// A volatile 32-bit read of a register.
///
/// The address is one of this module's register addresses: a memory-mapped
/// register of the RP2350 named by the datasheet's address map, word
/// aligned, aliased by no Rust reference, in a block whose atomic set and
/// clear aliases are used wherever a single bit moves; a volatile access
/// is the only way the hardware is reached.
fn rd(addr: usize) -> u32 {
    unsafe { read_volatile(addr as *const u32) }
}

/// A volatile 32-bit write; the contract of [`rd`].
fn wr(addr: usize, value: u32) {
    unsafe { write_volatile(addr as *mut u32, value) }
}

/// A SIO output's level (the set and clear registers, one bit each).
fn sio_write(pin: u32, high: bool) {
    let bit = 1u32 << pin;
    if high {
        wr(SIO + SIO_GPIO_OUT_SET, bit);
    } else {
        wr(SIO + SIO_GPIO_OUT_CLR, bit);
    }
}

/// A SIO output's enable.
fn sio_enable(pin: u32, output: bool) {
    let bit = 1u32 << pin;
    if output {
        wr(SIO + SIO_GPIO_OE_SET, bit);
    } else {
        wr(SIO + SIO_GPIO_OE_CLR, bit);
    }
}

/// A pin brought up in the datasheet's order (sections 9.3, 9.7, 9.10.1):
/// the pad configured with its isolation latch still set, the function
/// selected (with any override), the latch released.
fn pin_setup(pin: u32, control: u32, pad_bits: u32) {
    wr(pad(pin), pad_bits | PAD_ISO);
    wr(gpio_ctrl(pin), control);
    wr(pad(pin) + ATOMIC_CLR, PAD_ISO);
}

/// PIO0 state machine 0 as the gSPI wire on the Pico family's four pins.
#[derive(Debug)]
pub struct PioWire {
    clk_sys_hz: u32,
    ladder: Ladder,
    selected: (u8, i8),
    up: bool,
    fault: Option<Refusal>,
    armed: bool,
    frames: u32,
    pauses: u32,
}

impl PioWire {
    /// An engine on a system clock of `clk_sys_hz` (the PIO runs on the
    /// system clock; the consumer's clock tree sets it). Nothing is touched
    /// until [`PioWire::bring_up`], which the first strap performs if the
    /// caller did not.
    pub const fn new(clk_sys_hz: u32) -> Self {
        PioWire {
            clk_sys_hz,
            ladder: ladder(clk_sys_hz),
            selected: (0, PHASE_DEFAULT),
            up: false,
            fault: None,
            armed: false,
            frames: 0,
            pauses: 0,
        }
    }

    /// The system clock the engine was built for.
    pub fn clk_sys_hz(&self) -> u32 {
        self.clk_sys_hz
    }

    /// The settings ladder: the distinct dividers, slowest first.
    pub fn ladder(&self) -> Ladder {
        self.ladder
    }

    /// The clock divider in force.
    pub fn divider(&self) -> u32 {
        self.ladder.divider(self.selected.0)
    }

    /// The bit rate in force, in hertz.
    pub fn bit_hz(&self) -> u32 {
        super::bit_hz(self.clk_sys_hz, self.divider())
    }

    /// The frames clocked since bring-up.
    pub fn frames(&self) -> u32 {
        self.frames
    }

    /// The frames during which the caller drained the receive FIFO slower
    /// than the bus and the machine waited with the clock stopped: pauses,
    /// never errors.
    pub fn pauses(&self) -> u32 {
        self.pauses
    }

    /// Whether the data line's level-high interrupt is armed at the IO bank.
    pub fn irq_armed(&self) -> bool {
        self.armed
    }

    /// Arm or disarm the data line's level-high interrupt at the IO bank
    /// (processor 0's enable register). The engine disarms it around its
    /// own frames and restores it after. The interrupt line into the
    /// processor's interrupt controller, `IO_IRQ_BANK0`, is the caller's to
    /// enable and handle.
    pub fn arm_irq(&mut self, on: bool) {
        self.armed = on;
        self.irq_enable(on);
    }

    fn irq_enable(&self, on: bool) {
        let alias = if on { ATOMIC_SET } else { ATOMIC_CLR };
        wr(IO_BANK0 + PROC0_INTE3 + alias, GPIO24_LEVEL_HIGH);
    }

    /// The pins, the reset controller and the state machine brought up;
    /// idempotent. The power line is driven low, the select high, the data
    /// line low (the strap) and the clock low; the machine is parked on the
    /// default setting.
    pub fn bring_up(&mut self) -> Result<(), Refusal> {
        if self.up {
            return Ok(());
        }
        if let Some(fault) = self.fault {
            return Err(fault);
        }
        let result = self.bring_up_inner();
        match result {
            Ok(()) => self.up = true,
            Err(fault) => self.fault = Some(fault),
        }
        result
    }

    fn ensure_up(&mut self) {
        if !self.up && self.fault.is_none() {
            let _ = self.bring_up();
        }
    }

    fn bring_up_inner(&mut self) -> Result<(), Refusal> {
        // The reset controller: the IO bank, the pad bank and PIO0 released
        // through the reset register's atomic clear alias (section 2.1.3
        // gives every peripheral block the aliases but the SIO, the
        // CoreSight window, the private peripheral bus and the OTP bridge;
        // section 7.5), so a bit the caller's own setup moves in the same
        // register is never lost; then the reset-done flags awaited.
        let mask = RESET_IO_BANK0 | RESET_PADS_BANK0 | RESET_PIO0;
        wr(RESETS + RESET + ATOMIC_CLR, mask);
        let mut spins = 0u32;
        while rd(RESETS + RESET_DONE) & mask != mask {
            spins += 1;
            if spins > RESET_SPIN {
                return Err(Refusal::new(STAGE_RESET, rd(RESETS + RESET_DONE)));
            }
        }

        // The power line: an SIO output, low.
        sio_write(PIN_POWER, false);
        sio_enable(PIN_POWER, true);
        pin_setup(PIN_POWER, FUNCSEL_SIO, PAD_CONTROL_LINE);
        // The select: an SIO output, high.
        sio_write(PIN_SELECT, true);
        sio_enable(PIN_SELECT, true);
        pin_setup(PIN_SELECT, FUNCSEL_SIO, PAD_CONTROL_LINE);
        // The data line: an SIO output driven low across the module's
        // power-on (the interface-mode strap), handed to the PIO when the
        // strap is released.
        sio_write(PIN_DATA, false);
        sio_enable(PIN_DATA, true);
        pin_setup(PIN_DATA, FUNCSEL_SIO, PAD_DATA_LINE);
        // The clock: PIO0's, its output enable forced at the IO bank.
        pin_setup(PIN_CLOCK, FUNCSEL_PIO0 | OEOVER_ENABLE, PAD_CLOCK_LINE);

        // The state machine: disabled, the program for the default phase
        // loaded, the divider, the execution and shift controls, the pin
        // control, the synchroniser bypassed on the data line, then reset
        // and parked.
        wr(PIO0 + PIO_CTRL + ATOMIC_CLR, CTRL_SM0_ENABLE);
        self.selected = (0, PHASE_DEFAULT);
        self.load(PHASE_DEFAULT);
        wr(PIO0 + PIO_SM0_CLKDIV, clkdiv_word(self.divider()));
        wr(PIO0 + PIO_SM0_EXECCTRL, execctrl_word());
        wr(PIO0 + PIO_SM0_SHIFTCTRL, shiftctrl_word());
        wr(PIO0 + PIO_SM0_PINCTRL, pinctrl_word());
        wr(PIO0 + PIO_INPUT_SYNC_BYPASS + ATOMIC_SET, 1 << PIN_DATA);
        self.machine_reset();
        Ok(())
    }

    /// The program for a phase written to instruction memory (the machine
    /// disabled by the caller).
    fn load(&self, phase: i8) {
        let program = program(phase);
        for (i, word) in program.words[..program.len as usize].iter().enumerate() {
            wr(PIO0 + PIO_INSTR_MEM0 + 4 * i, u32::from(*word));
        }
    }

    /// The machine disabled, its FIFOs flushed (the join bit toggled twice
    /// discards their contents, section 11.5.3), its debug flags cleared,
    /// its state and divider restarted, the data line made an input and the
    /// program counter parked at 0 by forced instructions, then enabled.
    fn machine_reset(&self) {
        wr(PIO0 + PIO_CTRL + ATOMIC_CLR, CTRL_SM0_ENABLE);
        wr(PIO0 + PIO_SM0_SHIFTCTRL + ATOMIC_XOR, SHIFTCTRL_FJOIN_RX);
        wr(PIO0 + PIO_SM0_SHIFTCTRL + ATOMIC_XOR, SHIFTCTRL_FJOIN_RX);
        wr(PIO0 + PIO_FDEBUG, FDEBUG_SM0_ALL);
        wr(
            PIO0 + PIO_CTRL + ATOMIC_SET,
            CTRL_SM0_RESTART | CTRL_SM0_CLKDIV_RESTART,
        );
        wr(PIO0 + PIO_SM0_INSTR, INSTR_DATA_INPUT);
        wr(PIO0 + PIO_SM0_INSTR, INSTR_PARK);
        wr(PIO0 + PIO_CTRL + ATOMIC_SET, CTRL_SM0_ENABLE);
    }

    fn push(&self, word: u32) -> Result<(), Refusal> {
        let mut spins = 0u32;
        loop {
            let fstat = rd(PIO0 + PIO_FSTAT);
            if fstat & FSTAT_TXFULL0 == 0 {
                break;
            }
            spins += 1;
            if spins > FIFO_SPIN {
                return Err(Refusal::new(STAGE_TX_FIFO, fstat));
            }
        }
        wr(PIO0 + PIO_TXF0, word);
        Ok(())
    }

    fn pop(&self) -> Result<u32, Refusal> {
        let mut spins = 0u32;
        loop {
            let fstat = rd(PIO0 + PIO_FSTAT);
            if fstat & FSTAT_RXEMPTY0 == 0 {
                break;
            }
            spins += 1;
            if spins > FIFO_SPIN {
                return Err(Refusal::new(STAGE_RX_FIFO, fstat));
            }
        }
        Ok(rd(PIO0 + PIO_RXF0))
    }

    /// The frame's words through the FIFOs, then the park.
    fn frame_inner(&self, cmd: [u8; 4], tx: &[u8], rx: &mut [u8]) -> Result<(), Refusal> {
        let tx_words = 1 + tx.len().div_ceil(4);
        let rx_words = rx.len().div_ceil(4);
        self.push(header_x(tx_words))?;
        self.push(header_y(rx_words))?;
        self.push(u32::from_be_bytes(cmd))?;
        for chunk in tx.chunks(4) {
            let mut bytes = [0u8; 4];
            bytes[..chunk.len()].copy_from_slice(chunk);
            self.push(u32::from_be_bytes(bytes))?;
        }
        let mut at = 0usize;
        for _ in 0..rx_words {
            for b in self.pop()?.to_be_bytes() {
                if at < rx.len() {
                    rx[at] = b;
                }
                at += 1;
            }
        }
        let mut spins = 0u32;
        loop {
            let fstat = rd(PIO0 + PIO_FSTAT);
            let parked = fstat & FSTAT_TXEMPTY0 != 0 && rd(PIO0 + PIO_SM0_ADDR) & 0x1F == 0;
            if parked {
                if fstat & FSTAT_RXEMPTY0 == 0 {
                    return Err(Refusal::new(STAGE_RESPONSE_LEFT, fstat));
                }
                return Ok(());
            }
            spins += 1;
            if spins > FIFO_SPIN {
                return Err(Refusal::new(STAGE_PARK, fstat));
            }
        }
    }
}

impl GspiWire for PioWire {
    /// The power line driven (the SIO's set and clear registers).
    fn set_power(&mut self, on: bool) {
        self.ensure_up();
        sio_write(PIN_POWER, on);
    }

    /// The strap held: the data line an SIO output driven low, and the
    /// engine returned to its default setting. Released: the SIO output
    /// enable cleared and the line handed to PIO0, whose pin direction for
    /// it is input, so the pull-down holds it until the chip speaks.
    fn strap(&mut self, hold: bool) {
        self.ensure_up();
        if hold {
            if self.up && self.selected != (0, PHASE_DEFAULT) {
                self.select(0, PHASE_DEFAULT);
            }
            sio_write(PIN_DATA, false);
            sio_enable(PIN_DATA, true);
            wr(gpio_ctrl(PIN_DATA), FUNCSEL_SIO);
        } else {
            sio_enable(PIN_DATA, false);
            wr(gpio_ctrl(PIN_DATA), FUNCSEL_PIO0);
        }
    }

    /// One frame: the interrupt disarmed if it was armed, the debug flags
    /// cleared, the select driven low, the header words then the command
    /// and the data pushed as big-endian words (zeros after the data), the
    /// response words popped into the caller's slice (the last word's extra
    /// bytes discarded), the machine checked parked with nothing left in
    /// the receive FIFO, the select driven high, the interrupt restored. A
    /// refusal resets the machine first.
    fn frame(&mut self, cmd: [u8; 4], tx: &[u8], rx: &mut [u8]) -> Result<(), Refusal> {
        if let Some(fault) = self.fault {
            return Err(fault);
        }
        if !self.up {
            return Err(Refusal::new(STAGE_BRING_UP, 0));
        }
        let armed = self.armed;
        if armed {
            self.irq_enable(false);
        }
        wr(PIO0 + PIO_FDEBUG, FDEBUG_SM0_ALL);
        self.frames = self.frames.wrapping_add(1);
        sio_write(PIN_SELECT, false);
        let result = self.frame_inner(cmd, tx, rx);
        match result {
            Ok(()) => {
                if rd(PIO0 + PIO_FDEBUG) & FDEBUG_RXSTALL0 != 0 {
                    self.pauses = self.pauses.wrapping_add(1);
                }
            }
            Err(_) => self.machine_reset(),
        }
        sio_write(PIN_SELECT, true);
        if armed {
            self.irq_enable(true);
        }
        result
    }

    /// The data line's level from the SIO's input register, which reads
    /// the pad in every function.
    fn irq_asserted(&mut self) -> bool {
        rd(SIO + SIO_GPIO_IN) & (1 << PIN_DATA) != 0
    }

    fn settings(&self) -> u8 {
        self.ladder.count
    }

    fn setting(&self, index: u8) -> Setting {
        if index >= self.ladder.count {
            return Setting::NONE;
        }
        setting_of(self.clk_sys_hz, self.ladder.divider(index))
    }

    /// The divider and the program for the phase loaded with the machine
    /// disabled, then the machine reset and parked; an index or phase
    /// outside the ladder or the family is clamped.
    fn select(&mut self, index: u8, phase: i8) {
        let index = index.min(self.ladder.count.saturating_sub(1));
        let phase = phase.clamp(PHASE_FIRST, PHASE_LAST);
        self.selected = (index, phase);
        if !self.up {
            return;
        }
        wr(PIO0 + PIO_CTRL + ATOMIC_CLR, CTRL_SM0_ENABLE);
        self.load(phase);
        wr(PIO0 + PIO_SM0_CLKDIV, clkdiv_word(self.divider()));
        self.machine_reset();
    }

    fn selected(&self) -> (u8, i8) {
        self.selected
    }
}
