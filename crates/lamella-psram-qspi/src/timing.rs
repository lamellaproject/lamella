//! The memory window's timing and format words, derived from a part and the
//! system clock. The field layout is the RP2350's (the datasheet's QMI
//! chapter, section 12.14: the Mx_TIMING register of table 1297, the
//! Mx_RFMT and Mx_WFMT registers of tables 1298 and 1300, the Mx_RCMD and
//! Mx_WCMD registers of tables 1299 and 1301); the derivation is the
//! part's. Every division rounds toward safety, and a posture at which the
//! part's chip-select limit cannot be bounded is refused.

use crate::part::{Grade, Part};

/// The allowance for the host's pad delays and the board's trace, both
/// ways, added to the part's clock-to-output delay when the read sample
/// delay is chosen. A stated figure, not a tabulated one: the flight's
/// transcript measures the margin.
pub const PAD_ROUND_TRIP_PS: u32 = 3_000;

/// The host's cache line, the largest single transfer the window serves.
const CACHE_LINE_BYTES: u32 = 8;

/// A four-line transfer's bits per clock.
const QUAD_BITS_PER_CYCLE: u32 = 4;

/// The window's timing fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowTiming {
    /// System clock cycles per serial clock cycle (1 to 255).
    pub clkdiv: u8,
    /// Extra delay before the read data is sampled, in half system clock
    /// cycles (0 to 7).
    pub rxdelay: u8,
    /// Extra system clock cycles the chip select stays deasserted (0 to 31),
    /// beyond the half serial cycle the host always gives.
    pub min_deselect: u8,
    /// The longest chip-select assertion, in units of 64 system clock
    /// cycles (1 to 63; 0 would be unbounded and is never produced).
    pub max_select: u8,
    /// Extra system clock cycles of chip-select hold after the last clock
    /// edge (0 to 3).
    pub select_hold: u8,
    /// One extra system clock cycle of setup before the first clock edge.
    pub select_setup: bool,
    /// The page boundary a burst never crosses: 0 none, 1 = 256 bytes,
    /// 2 = 1024 bytes, 3 = 4096 bytes.
    pub pagebreak: u8,
    /// Chip-select hold after a transfer, in units of 64 system clock
    /// cycles, during which a sequential transfer is chained (0 to 3).
    pub cooldown: u8,
}

impl WindowTiming {
    /// The register word.
    pub const fn encode(&self) -> u32 {
        ((self.cooldown as u32) << 30)
            | ((self.pagebreak as u32) << 28)
            | ((self.select_setup as u32) << 25)
            | ((self.select_hold as u32) << 23)
            | ((self.max_select as u32) << 17)
            | ((self.min_deselect as u32) << 12)
            | ((self.rxdelay as u32) << 8)
            | (self.clkdiv as u32)
    }
}

/// Why no timing exists for a part at a system clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimingError {
    /// A system clock of zero.
    ClockZero,
    /// The divisor that meets the part's burst rate does not fit the field.
    DivisorRange,
    /// The chip-select assertion cannot be bounded inside the part's limit:
    /// one unit of the host's bound plus the worst transfer in flight would
    /// pass it. The limit and the transfer, both in system clock cycles.
    SelectBound { tcem_cycles: u32, tail_cycles: u32 },
    /// The deselect the part needs does not fit the field.
    DeselectRange,
    /// The sample delay the part's output delay needs does not fit the field.
    SampleDelayRange,
}

const fn div_ceil(a: u64, b: u64) -> u64 {
    (a + b - 1) / b
}

/// The window's timing for a part at a system clock and a grade.
///
/// - The divisor is the smallest that keeps the serial clock at or under
///   the part's linear-burst rate.
/// - The page break is the part's page; the cooldown chains sequential
///   transfers inside one assertion; no extra setup.
/// - The hold is the field's maximum: the part recommends a hold past its
///   output delay plus one clock for the last read data's window.
/// - The deselect is the part's minimum less the half serial cycle the host
///   gives, plus one cycle of margin.
/// - The assertion bound is the part's limit less the worst transfer that
///   may be in flight when the bound is reached (a four-line command and
///   address, the read's wait cycles, one cache line of data, the host's
///   half-cycle and hold tails), in units of 64 cycles, rounded down; a
///   result of zero is refused, because zero means unbounded.
/// - The sample delay is the part's output delay plus the pad allowance,
///   less the half serial cycle, in half system cycles, rounded up, PLUS
///   ONE half system cycle of margin: measurements on both Pimoroni
///   boards found the rounded-up value to be the lower edge of the
///   working range (one step below it fails at the first word; one and
///   two steps above it hold), so the posture sits one step inside that
///   range. A clock whose derived delay already fills the field has no
///   room for the margin and is refused.
/// The margin above the derived sample delay, in half system cycles (see
/// `window_timing`).
pub const RXDELAY_MARGIN: u64 = 1;

pub fn window_timing(part: &Part, sys_hz: u32, grade: Grade) -> Result<WindowTiming, TimingError> {
    if sys_hz == 0 {
        return Err(TimingError::ClockZero);
    }
    let sys_hz64 = sys_hz as u64;

    let clkdiv = div_ceil(sys_hz64, part.max_linear_hz as u64).max(1);
    if clkdiv > 255 {
        return Err(TimingError::DivisorRange);
    }

    // The assertion bound, in system clock cycles: the limit rounded down,
    // the in-flight tail rounded up.
    let tcem_cycles = (part.tcem_ns(grade) as u64 * sys_hz64) / 1_000_000_000;
    let prefix_cycles = 8 / QUAD_BITS_PER_CYCLE;
    let address_cycles = 24 / QUAD_BITS_PER_CYCLE;
    let data_cycles = CACHE_LINE_BYTES * 8 / QUAD_BITS_PER_CYCLE;
    let serial_cycles = prefix_cycles + address_cycles + part.fast_read_quad_wait_cycles as u32 + data_cycles;
    let tail_cycles = serial_cycles as u64 * clkdiv + 8;
    if tcem_cycles <= tail_cycles {
        return Err(TimingError::SelectBound { tcem_cycles: tcem_cycles as u32, tail_cycles: tail_cycles as u32 });
    }
    let max_select = (tcem_cycles - tail_cycles) / 64;
    if max_select == 0 {
        return Err(TimingError::SelectBound { tcem_cycles: tcem_cycles as u32, tail_cycles: tail_cycles as u32 });
    }
    let max_select = max_select.min(63);

    // The deselect: the part's minimum less the host's half serial cycle
    // (rounded up to whole system cycles), plus one cycle of margin.
    let tcph_cycles = div_ceil(part.tcph_ns as u64 * sys_hz64, 1_000_000_000);
    let half_sck_cycles = div_ceil(clkdiv, 2);
    let min_deselect = tcph_cycles.saturating_sub(half_sck_cycles) + 1;
    if min_deselect > 31 {
        return Err(TimingError::DeselectRange);
    }

    // The sample delay, in half system cycles.
    let half_sys_ps = 500_000_000_000u64 / sys_hz64;
    let half_sck_ps = half_sys_ps * clkdiv;
    let need_ps = part.taclk_max_ps as u64 + PAD_ROUND_TRIP_PS as u64;
    let derived = if need_ps > half_sck_ps { div_ceil(need_ps - half_sck_ps, half_sys_ps) } else { 0 };
    let rxdelay = derived + RXDELAY_MARGIN;
    if rxdelay > 7 {
        return Err(TimingError::SampleDelayRange);
    }

    Ok(WindowTiming {
        clkdiv: clkdiv as u8,
        rxdelay: rxdelay as u8,
        min_deselect: min_deselect as u8,
        max_select: max_select as u8,
        select_hold: 3,
        select_setup: false,
        pagebreak: pagebreak_code(part.page_bytes),
        cooldown: 1,
    })
}

/// The page-break field for a page size: the next larger boundary the field
/// offers when the page is not one of its three.
const fn pagebreak_code(page_bytes: u32) -> u8 {
    if page_bytes == 0 {
        0
    } else if page_bytes <= 256 {
        1
    } else if page_bytes <= 1024 {
        2
    } else {
        3
    }
}

/// The direct-mode clock divisor: the smallest that keeps the serial clock
/// at or under 10 MHz, inside the part's Read ID bound with margin.
pub fn direct_clkdiv(sys_hz: u32) -> u32 {
    (div_ceil(sys_hz as u64, 10_000_000) as u32).clamp(1, 255)
}

const WIDTH_QUAD: u32 = 2;

/// The read format word: a four-line 8-bit command, a four-line 24-bit
/// address, the part's wait cycles as a four-line dummy phase, four-line
/// data, no suffix, single transfer rate.
pub const fn read_format(part: &Part) -> u32 {
    ((part.fast_read_quad_wait_cycles as u32) << 16)
        | (1 << 12)
        | (WIDTH_QUAD << 8)
        | (WIDTH_QUAD << 6)
        | (WIDTH_QUAD << 4)
        | (WIDTH_QUAD << 2)
        | WIDTH_QUAD
}

/// The read command word: the part's fast quad read as the prefix.
pub const fn read_command(part: &Part) -> u32 {
    part.cmd_fast_read_quad as u32
}

/// The write format word: the read's shape without a dummy phase.
pub const fn write_format(_part: &Part) -> u32 {
    (1 << 12) | (WIDTH_QUAD << 8) | (WIDTH_QUAD << 6) | (WIDTH_QUAD << 4) | (WIDTH_QUAD << 2) | WIDTH_QUAD
}

/// The write command word: the part's quad write as the prefix.
pub const fn write_command(part: &Part) -> u32 {
    part.cmd_quad_write as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: &Part = &Part::APS6404L_3SQR;

    #[test]
    fn the_150_mhz_posture() {
        let t = window_timing(P, 150_000_000, Grade::Standard).unwrap();
        assert_eq!(t.clkdiv, 2, "75 MHz serial clock under the 84 MHz burst bound");
        assert_eq!(t.rxdelay, 2, "1.83 ns over the half cycle rounds up to 1; one half cycle of margin");
        assert_eq!(t.min_deselect, 3, "18 ns needs 2.7 cycles; one half serial cycle given; one of margin");
        assert_eq!(t.max_select, 17, "(1200 - 68) / 64");
        assert_eq!(t.select_hold, 3);
        assert!(!t.select_setup);
        assert_eq!(t.pagebreak, 2);
        assert_eq!(t.cooldown, 1);
        assert_eq!(t.encode(), 0x61A2_3202);
    }

    #[test]
    fn the_12_mhz_crystal_posture_is_refused() {
        // 96 cycles of limit, 38 of tail: one 64-cycle unit plus the tail
        // would pass the limit.
        assert_eq!(
            window_timing(P, 12_000_000, Grade::Standard),
            Err(TimingError::SelectBound { tcem_cycles: 96, tail_cycles: 38 })
        );
    }

    #[test]
    fn the_extended_grade_tightens_the_bound() {
        let t = window_timing(P, 150_000_000, Grade::Extended).unwrap();
        assert_eq!(t.max_select, 5, "(450 - 68) / 64, rounded down");
        assert_eq!(t.clkdiv, 2);
    }

    #[test]
    fn a_faster_clock_divides_further() {
        let t = window_timing(P, 200_000_000, Grade::Standard).unwrap();
        assert_eq!(t.clkdiv, 3, "66.7 MHz under the bound; 100 MHz over it");
        assert_eq!(t.max_select, 23, "(1600 - 98) / 64");
        assert_eq!(t.min_deselect, 3);
        assert_eq!(t.rxdelay, 2);
    }

    #[test]
    fn the_doctored_part_with_no_select_limit_is_refused() {
        let mut p = Part::APS6404L_3SQR;
        p.tcem_standard_ns = 0;
        assert!(matches!(window_timing(&p, 150_000_000, Grade::Standard), Err(TimingError::SelectBound { .. })));
    }

    #[test]
    fn a_zero_clock_is_refused() {
        assert_eq!(window_timing(P, 0, Grade::Standard), Err(TimingError::ClockZero));
    }

    #[test]
    fn the_format_and_command_words() {
        assert_eq!(read_format(P), 0x0006_12AA);
        assert_eq!(read_command(P), 0xEB);
        assert_eq!(write_format(P), 0x0000_12AA);
        assert_eq!(write_command(P), 0x38);
    }

    #[test]
    fn the_direct_mode_divisor() {
        assert_eq!(direct_clkdiv(150_000_000), 15);
        assert_eq!(direct_clkdiv(12_000_000), 2);
        assert_eq!(direct_clkdiv(1_000_000), 1);
        assert_eq!(direct_clkdiv(u32::MAX), 255);
    }
}
