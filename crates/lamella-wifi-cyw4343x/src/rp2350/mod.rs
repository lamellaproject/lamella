//! The RP2350's programmable I/O block as the gSPI engine for the Raspberry
//! Pi Pico 2 W and Pimoroni Pico Plus 2 W boards: PIO0 state machine 0
//! clocks the chip's serial interface on the boards' four pins (GPIO 23 the
//! module's power line, GPIO 24 the shared data line that is also the
//! chip's interrupt, GPIO 25 the select, GPIO 29 the clock), with the
//! sample instant and the bit rate as settings the transport tunes.
//!
//! The four lines are the boards' own facts, the same on both. The
//! Raspberry Pi Pico 2 W's published board description states them as the
//! module's four device rows through the single-cycle I/O block: the power
//! enable (high to power), the data line, the select (low to select) and
//! the clock. The Pimoroni Pico Plus 2 W's schematic (sheet 1 of 3, the
//! RP2350B MCU section and the RM2 module section) draws them as the nets
//! `WL_ON` from GPIO 23 into the module's `WLON` pin, `WL_D` from GPIO 24
//! into its `DI` pin and its `IRQ` pin (the data line doubling as the
//! interrupt), `WL_CS` from GPIO 25 into its `CS` pin and `WL_CLK` from
//! GPIO 29 into its `SCLK` pin; the board's LED hangs on the module's own
//! `GPIO0`, not on a chip line.
//!
//! Register facts per the RP2350 datasheet: section 2.1.3 (the atomic
//! register aliases), 2.2 (the address map), 3.1 (the SIO's GPIO
//! registers), 7.5 (the subsystem resets), chapter 9 (the GPIO function
//! select, the pads, the isolation latches, the interrupts) and chapter 11
//! (the PIO's programmer's model, its instruction encodings and its
//! registers). Bus facts per the Infineon CYW43439 datasheet, section 4.2
//! (the protocol and its word alignment) and section 18.3 (the timing: the
//! device samples on the rising edge; 5 ns of input hold). The four-pin
//! wiring, the data line doubling as the interrupt, the strap held through
//! the data line, and the chip turning its driver on 20 to 26 ns after each
//! rising edge are measured facts of the boards and the silicon.
//!
//! Every bit takes eight cycles of the state machine's clock: the rising
//! edge at cycle 0, the falling edge at cycle 4. The host drives its bits
//! at the falling edge and releases the data line one cycle after the last
//! command edge; the read loop samples at a phase of the bit period chosen
//! by the tuning, from three cycles before the rising edge to four after
//! it. The pure parts -- the register facts, the program family, the
//! encodings, the settings ladder, the control words -- are here; the engine
//! that touches the hardware is [`PioWire`].

mod engine;

pub use engine::PioWire;

use crate::gspi::Setting;

/// The module's power-enable line, high to power: the Pico 2 W's
/// description's power-enable row; the Plus 2 W's net `WL_ON` (GPIO 23,
/// sheet 1) into the module's `WLON` pin.
pub const PIN_POWER: u32 = 23;
/// The shared data line, and the chip's interrupt while the bus is idle:
/// the Pico 2 W's data row; the Plus 2 W's net `WL_D` (GPIO 24, sheet 1)
/// into the module's `DI` and `IRQ` pins.
pub const PIN_DATA: u32 = 24;
/// The select, active low: the Pico 2 W's select row; the Plus 2 W's net
/// `WL_CS` (GPIO 25, sheet 1) into the module's `CS` pin.
pub const PIN_SELECT: u32 = 25;
/// The clock: the Pico 2 W's clock row; the Plus 2 W's net `WL_CLK`
/// (GPIO 29, sheet 1) into the module's `SCLK` pin.
pub const PIN_CLOCK: u32 = 29;

/// The reset controller (the address map; section 7.5.3).
pub const RESETS: usize = 0x4002_0000;
/// The reset register.
pub const RESET: usize = 0x000;
/// The reset-done register.
pub const RESET_DONE: usize = 0x008;
/// The IO bank's bit in the reset registers.
pub const RESET_IO_BANK0: u32 = 1 << 6;
/// The pad bank's bit.
pub const RESET_PADS_BANK0: u32 = 1 << 9;
/// PIO0's bit.
pub const RESET_PIO0: u32 = 1 << 11;

/// The IO bank's registers (section 9.11.1).
pub const IO_BANK0: usize = 0x4002_8000;
/// The pad bank's registers (section 9.11.3).
pub const PADS_BANK0: usize = 0x4003_8000;
/// The single-cycle IO block (section 3.1).
pub const SIO: usize = 0xd000_0000;
/// PIO0's registers (section 11.7).
pub const PIO0: usize = 0x5020_0000;

/// The atomic aliases of a peripheral's registers (section 2.1.3).
pub const ATOMIC_XOR: usize = 0x1000;
/// The bit-set alias.
pub const ATOMIC_SET: usize = 0x2000;
/// The bit-clear alias.
pub const ATOMIC_CLR: usize = 0x3000;

/// A GPIO's control register in the IO bank (Table 649: the status and
/// control pair, eight bytes per pin).
pub const fn gpio_ctrl(pin: u32) -> usize {
    IO_BANK0 + 0x004 + 8 * pin as usize
}

/// A GPIO's pad register (Table 875 and following: four bytes per pin
/// after the voltage select).
pub const fn pad(pin: u32) -> usize {
    PADS_BANK0 + 0x004 + 4 * pin as usize
}

/// The control register's function select: the SIO (Table 645, F5).
pub const FUNCSEL_SIO: u32 = 5;
/// The control register's function select: PIO0 (F6).
pub const FUNCSEL_PIO0: u32 = 6;
/// The control register's output-enable override forced on (Table 699,
/// `OEOVER` = 3).
pub const OEOVER_ENABLE: u32 = 3 << 14;

/// The interrupt-enable register for processor 0 covering GPIOs 24 to 31
/// (section 9.11.1; four bits per pin: level low, level high, edge low,
/// edge high).
pub const PROC0_INTE3: usize = 0x254;
/// GPIO 24's level-high event in that register.
pub const GPIO24_LEVEL_HIGH: u32 = 1 << 1;

/// Pad control: the isolation latch (Table 877, bit 8).
pub const PAD_ISO: u32 = 1 << 8;
/// Pad control: the output disable.
pub const PAD_OD: u32 = 1 << 7;
/// Pad control: the input enable.
pub const PAD_IE: u32 = 1 << 6;
/// Pad control: 4 mA drive.
pub const PAD_DRIVE_4MA: u32 = 1 << 4;
/// Pad control: 12 mA drive.
pub const PAD_DRIVE_12MA: u32 = 3 << 4;
/// Pad control: the pull-up.
pub const PAD_PUE: u32 = 1 << 3;
/// Pad control: the pull-down.
pub const PAD_PDE: u32 = 1 << 2;
/// Pad control: the input hysteresis.
pub const PAD_SCHMITT: u32 = 1 << 1;
/// Pad control: the fast slew rate.
pub const PAD_SLEWFAST: u32 = 1 << 0;

/// The power line's and the select's pad: input enabled, 4 mA, no pull,
/// hysteresis, slow slew.
pub const PAD_CONTROL_LINE: u32 = PAD_IE | PAD_DRIVE_4MA | PAD_SCHMITT;
/// The data line's pad: input enabled, 4 mA, pulled DOWN so a released
/// line reads low and high while idle is the chip's interrupt, hysteresis,
/// slow slew.
pub const PAD_DATA_LINE: u32 = PAD_IE | PAD_DRIVE_4MA | PAD_PDE | PAD_SCHMITT;
/// The clock's pad: input enabled, 12 mA, pulled down, fast slew.
pub const PAD_CLOCK_LINE: u32 = PAD_IE | PAD_DRIVE_12MA | PAD_PDE | PAD_SLEWFAST;

/// SIO: the GPIO input register (Table 17).
pub const SIO_GPIO_IN: usize = 0x004;
/// SIO: the output set register.
pub const SIO_GPIO_OUT_SET: usize = 0x018;
/// SIO: the output clear register.
pub const SIO_GPIO_OUT_CLR: usize = 0x020;
/// SIO: the output-enable set register.
pub const SIO_GPIO_OE_SET: usize = 0x038;
/// SIO: the output-enable clear register.
pub const SIO_GPIO_OE_CLR: usize = 0x040;

/// PIO: the control register (Table 982).
pub const PIO_CTRL: usize = 0x000;
/// PIO: the FIFO status register (Table 983).
pub const PIO_FSTAT: usize = 0x004;
/// PIO: the FIFO debug register (Table 984).
pub const PIO_FDEBUG: usize = 0x008;
/// PIO: state machine 0's transmit FIFO (Table 986).
pub const PIO_TXF0: usize = 0x010;
/// PIO: state machine 0's receive FIFO (Table 987).
pub const PIO_RXF0: usize = 0x020;
/// PIO: the input synchroniser bypass (Table 990).
pub const PIO_INPUT_SYNC_BYPASS: usize = 0x038;
/// PIO: the first instruction memory word (Table 994).
pub const PIO_INSTR_MEM0: usize = 0x048;
/// PIO: state machine 0's clock divider (Table 995).
pub const PIO_SM0_CLKDIV: usize = 0x0c8;
/// PIO: state machine 0's execution control (Table 996).
pub const PIO_SM0_EXECCTRL: usize = 0x0cc;
/// PIO: state machine 0's shift control (Table 997).
pub const PIO_SM0_SHIFTCTRL: usize = 0x0d0;
/// PIO: state machine 0's program counter (Table 998).
pub const PIO_SM0_ADDR: usize = 0x0d4;
/// PIO: state machine 0's forced instruction (Table 999).
pub const PIO_SM0_INSTR: usize = 0x0d8;
/// PIO: state machine 0's pin control (Table 1000).
pub const PIO_SM0_PINCTRL: usize = 0x0dc;

/// Control: state machine 0 enabled.
pub const CTRL_SM0_ENABLE: u32 = 1 << 0;
/// Control: state machine 0 restarted.
pub const CTRL_SM0_RESTART: u32 = 1 << 4;
/// Control: state machine 0's clock divider restarted.
pub const CTRL_SM0_CLKDIV_RESTART: u32 = 1 << 8;

/// FIFO status: state machine 0's receive FIFO empty.
pub const FSTAT_RXEMPTY0: u32 = 1 << 8;
/// FIFO status: state machine 0's transmit FIFO full.
pub const FSTAT_TXFULL0: u32 = 1 << 16;
/// FIFO status: state machine 0's transmit FIFO empty.
pub const FSTAT_TXEMPTY0: u32 = 1 << 24;

/// FIFO debug: state machine 0 stalled on a full receive FIFO.
pub const FDEBUG_RXSTALL0: u32 = 1 << 0;
/// FIFO debug: every flag of state machine 0 (write 1 to clear).
pub const FDEBUG_SM0_ALL: u32 = (1 << 24) | (1 << 16) | (1 << 8) | 1;

/// Shift control: the receive FIFO takes the transmit FIFO's storage;
/// toggled twice, it flushes both FIFOs.
pub const SHIFTCTRL_FJOIN_RX: u32 = 1 << 31;
/// Shift control: autopull.
pub const SHIFTCTRL_AUTOPULL: u32 = 1 << 17;
/// Shift control: autopush.
pub const SHIFTCTRL_AUTOPUSH: u32 = 1 << 16;

/// The cycles of the state machine's clock in one bit, both directions.
pub const CYCLES_PER_BIT: u8 = 8;
/// The earliest sample phase, in cycles relative to the rising edge.
pub const PHASE_FIRST: i8 = -3;
/// The number of sample phases: one bit period.
pub const PHASE_COUNT: u8 = 8;
/// The last sample phase.
pub const PHASE_LAST: i8 = PHASE_FIRST + PHASE_COUNT as i8 - 1;
/// The phase the engine starts at, inside the clean run measured at the
/// slowest rate of the ladder.
pub const PHASE_DEFAULT: i8 = -2;
/// The most words a program of the family takes.
pub const PROGRAM_WORDS: usize = 14;

/// The instruction encodings (Table 980 and the instruction sections of
/// 11.4): the opcode in bits 15:13, the side-set value in bit 12 and the
/// delay in bits 11:8 for a one-bit side-set without an enable, the
/// operands below.
pub mod instr {
    /// A jump: `0x0000 | condition << 5 | address`.
    pub const fn jmp(condition: u16, address: u16) -> u16 {
        (condition << 5) | (address & 31)
    }
    /// Jump always.
    pub const JMP_ALWAYS: u16 = 0;
    /// Jump while X is non-zero, decrementing it.
    pub const JMP_X_DEC: u16 = 2;
    /// Jump if Y is zero.
    pub const JMP_NOT_Y: u16 = 3;
    /// Jump while Y is non-zero, decrementing it.
    pub const JMP_Y_DEC: u16 = 4;

    /// Shift `count` bits from the pins into the input shift register:
    /// `0x4000 | 0 << 5 | count` (32 encoded as 0).
    pub const fn in_pins(count: u16) -> u16 {
        0x4000 | (count & 31)
    }

    /// Shift `count` bits out of the output shift register to a
    /// destination: `0x6000 | destination << 5 | count`.
    pub const fn out(destination: u16, count: u16) -> u16 {
        0x6000 | (destination << 5) | (count & 31)
    }
    /// The OUT destination: the pins.
    pub const OUT_PINS: u16 = 0;
    /// The OUT destination: scratch X.
    pub const OUT_X: u16 = 1;
    /// The OUT destination: scratch Y.
    pub const OUT_Y: u16 = 2;

    /// Write an immediate to a destination: `0xE000 | destination << 5 |
    /// data`.
    pub const fn set(destination: u16, data: u16) -> u16 {
        0xE000 | (destination << 5) | (data & 31)
    }
    /// The SET destination: the pin directions.
    pub const SET_PINDIRS: u16 = 4;

    /// No operation: `MOV Y, Y`.
    pub const NOP: u16 = 0xA042;

    /// The side-set value 1: the clock high.
    pub const SIDE_1: u16 = 1 << 12;

    /// A delay of `cycles` after the instruction.
    pub const fn delay(cycles: u16) -> u16 {
        (cycles & 15) << 8
    }
}

/// A program of the family: its words and their count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Program {
    /// The words, from instruction memory 0.
    pub words: [u16; PROGRAM_WORDS],
    /// How many of them the program uses.
    pub len: u8,
}

/// The program for a sample phase (clamped into the family): the two
/// header words taken into X (the bits out, less two) and Y (the bits in),
/// the data line driven, the bits shifted out at the falling edge with the
/// rising edge four cycles later, the last bit unrolled so the data line is
/// released one cycle after its rising edge, then the read loop for the
/// phase -- edge-first for phases at or after the rising edge, sample-first
/// for phases before it -- whose test at its foot falls through to the exit
/// when Y reaches zero, and the machine parks at instruction 0.
pub const fn program(phase: i8) -> Program {
    use instr::*;
    let phase = if phase < PHASE_FIRST {
        PHASE_FIRST
    } else if phase > PHASE_LAST {
        PHASE_LAST
    } else {
        phase
    };
    let mut w = [0u16; PROGRAM_WORDS];
    w[0] = out(OUT_X, 32);
    w[1] = out(OUT_Y, 32);
    w[2] = set(SET_PINDIRS, 1);
    w[3] = out(OUT_PINS, 1) | delay(3);
    w[4] = jmp(JMP_X_DEC, 3) | SIDE_1 | delay(3);
    w[5] = out(OUT_PINS, 1) | delay(3);
    w[6] = NOP | SIDE_1;
    w[7] = set(SET_PINDIRS, 0) | SIDE_1 | delay(2);
    let len: u8;
    match phase {
        0 => {
            w[8] = jmp(JMP_ALWAYS, 11) | delay(2);
            w[9] = in_pins(1) | SIDE_1 | delay(3);
            w[10] = jmp(JMP_ALWAYS, 11) | delay(2);
            w[11] = jmp(JMP_Y_DEC, 9);
            w[12] = jmp(JMP_ALWAYS, 0);
            len = 13;
        }
        1..=3 => {
            let t = phase as u16;
            w[8] = jmp(JMP_ALWAYS, 12) | delay(2);
            w[9] = NOP | SIDE_1 | delay(t - 1);
            w[10] = in_pins(1) | SIDE_1 | delay(3 - t);
            w[11] = jmp(JMP_ALWAYS, 12) | delay(2);
            w[12] = jmp(JMP_Y_DEC, 9);
            w[13] = jmp(JMP_ALWAYS, 0);
            len = 14;
        }
        4 => {
            w[8] = jmp(JMP_ALWAYS, 12) | delay(2);
            w[9] = NOP | SIDE_1 | delay(3);
            w[10] = in_pins(1);
            w[11] = jmp(JMP_ALWAYS, 12) | delay(1);
            w[12] = jmp(JMP_Y_DEC, 9);
            w[13] = jmp(JMP_ALWAYS, 0);
            len = 14;
        }
        -1 => {
            w[8] = jmp(JMP_ALWAYS, 12) | delay(1);
            w[9] = in_pins(1);
            w[10] = NOP | SIDE_1 | delay(3);
            w[11] = jmp(JMP_ALWAYS, 12) | delay(1);
            w[12] = jmp(JMP_Y_DEC, 9);
            w[13] = jmp(JMP_ALWAYS, 0);
            len = 14;
        }
        -2 => {
            w[8] = jmp(JMP_ALWAYS, 12);
            w[9] = in_pins(1) | delay(1);
            w[10] = NOP | SIDE_1 | delay(3);
            w[11] = jmp(JMP_ALWAYS, 12);
            w[12] = jmp(JMP_Y_DEC, 9);
            w[13] = jmp(JMP_ALWAYS, 0);
            len = 14;
        }
        _ => {
            w[8] = jmp(JMP_ALWAYS, 11);
            w[9] = in_pins(1) | delay(2);
            w[10] = NOP | SIDE_1 | delay(3);
            w[11] = jmp(JMP_Y_DEC, 9);
            w[12] = jmp(JMP_ALWAYS, 0);
            len = 13;
        }
    }
    Program { words: w, len }
}

/// The instruction forced through `SM0_INSTR` to make the data line an
/// input: `SET PINDIRS, 0`.
pub const INSTR_DATA_INPUT: u32 = 0xE080;
/// The instruction forced through `SM0_INSTR` to park the machine: `JMP 0`.
pub const INSTR_PARK: u32 = 0x0000;

/// The target bit rates of the settings ladder, slowest first.
pub const RATES_HZ: [u32; 3] = [6_250_000, 9_375_000, 18_750_000];

/// The integer clock divider nearest `clk_sys / (8 x rate)`, a half rounding
/// up, at least 1 and at most 65535 (Table 995: the divider's integer
/// field; the fraction is left at zero so the cadence is exact).
pub const fn divider(clk_sys_hz: u32, rate_hz: u32) -> u32 {
    let eight = 8 * rate_hz as u64;
    let d = (clk_sys_hz as u64 + eight / 2) / eight;
    if d == 0 {
        1
    } else if d > 0xFFFF {
        0xFFFF
    } else {
        d as u32
    }
}

/// The settings ladder at a system clock: the distinct dividers of the
/// target rates, slowest first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ladder {
    /// The dividers, slowest first.
    pub dividers: [u32; 3],
    /// How many are distinct.
    pub count: u8,
}

/// The ladder at `clk_sys_hz`.
pub const fn ladder(clk_sys_hz: u32) -> Ladder {
    let mut dividers = [0u32; 3];
    let mut count = 0u8;
    let mut i = 0;
    while i < RATES_HZ.len() {
        let d = divider(clk_sys_hz, RATES_HZ[i]);
        if count == 0 || dividers[count as usize - 1] != d {
            dividers[count as usize] = d;
            count += 1;
        }
        i += 1;
    }
    Ladder { dividers, count }
}

impl Ladder {
    /// The `index`th divider.
    pub const fn divider(&self, index: u8) -> u32 {
        self.dividers[index as usize]
    }
}

/// The bit rate at a divider: the system clock over eight cycles per bit.
pub const fn bit_hz(clk_sys_hz: u32, divider: u32) -> u32 {
    clk_sys_hz / (CYCLES_PER_BIT as u32 * divider)
}

/// The tuning setting at a divider: the cycle in picoseconds (rounded to
/// nearest), eight steps per bit, the eight phases of the family.
pub const fn setting_of(clk_sys_hz: u32, divider: u32) -> Setting {
    let clk = clk_sys_hz as u64;
    let step_ps = match (divider as u64 * 1_000_000_000_000 + clk / 2).checked_div(clk) {
        None => 0,
        Some(step) => step,
    };
    Setting {
        step_ps: step_ps as u32,
        steps_per_bit: CYCLES_PER_BIT,
        first: PHASE_FIRST,
        phases: PHASE_COUNT,
    }
}

/// The clock divider word: the integer part in bits 31:16, no fraction.
pub const fn clkdiv_word(divider: u32) -> u32 {
    (divider & 0xFFFF) << 16
}

/// The execution control word: wrap top 31, wrap bottom 0 (the reset
/// values; the program jumps explicitly), no side-set enable bit, side-set
/// to the pin levels.
pub const fn execctrl_word() -> u32 {
    31 << 12
}

/// The shift control word: autopull and autopush at 32 bits, both shift
/// registers shifting left so the first byte on the wire is the most
/// significant byte of a FIFO word, the FIFOs not joined.
pub const fn shiftctrl_word() -> u32 {
    SHIFTCTRL_AUTOPULL | SHIFTCTRL_AUTOPUSH
}

/// The pin control word: one side-set pin at the clock, one SET pin and one
/// OUT pin at the data line, the IN base at the data line (Table 1000).
pub const fn pinctrl_word() -> u32 {
    (1 << 29)
        | (1 << 26)
        | (1 << 20)
        | (PIN_DATA << 15)
        | (PIN_CLOCK << 10)
        | (PIN_DATA << 5)
        | PIN_DATA
}

/// The transmit FIFO's first header word for a frame of `tx_words` words
/// out: the bits out less two.
pub const fn header_x(tx_words: usize) -> u32 {
    tx_words as u32 * 32 - 2
}

/// The transmit FIFO's second header word: the bits in.
pub const fn header_y(rx_words: usize) -> u32 {
    rx_words as u32 * 32
}
