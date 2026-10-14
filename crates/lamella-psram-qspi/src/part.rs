//! A part's datasheet facts as a value. The one part stated today is the AP
//! Memory APS6404L-3SQR (64 Mbit, 8 M x 8, SPI/QPI, 3 V), read from the
//! maker's datasheet "APM SPI 3V PSRAM Datasheet", Rev. 2.7:
//! the command table of section 9.5, the page size of 9.2, the power-on
//! state of 9.4, the known-good-die table of section 12, the reset
//! operation of section 14 and the AC table of section 16.7 (Table 10).
//! A compatible part from another maker is a second value of this type.

/// The temperature grade, which sets the longest time the chip select may
/// stay asserted (the part refreshes itself only while deselected).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Grade {
    /// -40 to +85 C.
    Standard,
    /// -40 to +105 C.
    Extended,
}

/// The facts the bring-up and the timing derivation read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Part {
    /// The maker's part number.
    pub name: &'static str,
    /// The part's size in bytes.
    pub bytes: usize,
    /// The page a burst may not cross at the full rate.
    pub page_bytes: u32,
    /// The highest clock a linear burst may run at; the window's clock is
    /// divided down to meet it.
    pub max_linear_hz: u32,
    /// The highest clock the Read ID command may run at (a serial-mode
    /// command); the direct-mode clock is divided down to meet it.
    pub read_id_max_hz: u32,
    /// The longest chip-select assertion, standard grade, in nanoseconds.
    pub tcem_standard_ns: u32,
    /// The longest chip-select assertion, extended grade, in nanoseconds.
    pub tcem_extended_ns: u32,
    /// The shortest chip-select deassertion between bursts, in nanoseconds.
    pub tcph_ns: u32,
    /// The longest clock-to-output delay, in picoseconds.
    pub taclk_max_ps: u32,
    /// The time after the reset command before the next command, in
    /// nanoseconds.
    pub trst_ns: u32,
    /// Read ID (serial mode only).
    pub cmd_read_id: u8,
    /// Reset enable, then reset, each its own chip-select frame.
    pub cmd_reset_enable: u8,
    pub cmd_reset: u8,
    /// Enter the four-line mode (serial mode only).
    pub cmd_enter_quad: u8,
    /// Leave the four-line mode (four-line mode only).
    pub cmd_exit_quad: u8,
    /// The fast read with a four-line command, address and data.
    pub cmd_fast_read_quad: u8,
    /// The wait cycles between the address and the data of that read.
    pub fast_read_quad_wait_cycles: u8,
    /// The write with a four-line command, address and data, no wait.
    pub cmd_quad_write: u8,
    /// The known-good-die byte a good part answers.
    pub kgd_pass: u8,
}

impl Part {
    /// The AP Memory APS6404L-3SQR, the part on the Pimoroni Pico Plus 2 and
    /// Pico Plus 2 W boards (their schematics: "PSRAM 8MB APS6404L-3SQR-ZR").
    pub const APS6404L_3SQR: Part = Part {
        name: "APS6404L-3SQR",
        bytes: 8 * 1024 * 1024,
        page_bytes: 1024,
        max_linear_hz: 84_000_000,
        read_id_max_hz: 33_000_000,
        tcem_standard_ns: 8_000,
        tcem_extended_ns: 3_000,
        tcph_ns: 18,
        taclk_max_ps: 5_500,
        trst_ns: 50,
        cmd_read_id: 0x9F,
        cmd_reset_enable: 0x66,
        cmd_reset: 0x99,
        cmd_enter_quad: 0x35,
        cmd_exit_quad: 0xF5,
        cmd_fast_read_quad: 0xEB,
        fast_read_quad_wait_cycles: 6,
        cmd_quad_write: 0x38,
        kgd_pass: 0x5D,
    };

    /// The longest chip-select assertion at a grade, in nanoseconds.
    pub const fn tcem_ns(&self, grade: Grade) -> u32 {
        match grade {
            Grade::Standard => self.tcem_standard_ns,
            Grade::Extended => self.tcem_extended_ns,
        }
    }
}
