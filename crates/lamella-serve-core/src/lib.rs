//! Shared plumbing for the interpreter firmware crates -- among it `startup.rs` (the bare-metal
//! Rust-runtime establishment every Cortex-M firmware binary's vector table enters through) and
//! `usb_transport.rs` (the driverless-WinUSB Lamella Link carrier, generic over the chip's device
//! controller).
//!
//! These files are `#[path]`-included by each firmware binary rather than compiled here as a
//! lib: they carry per-binary statics (`critical_section::set_impl!`, the frame readers) and
//! cfg on the including crate's features (`usb`), so each binary compiles its own copy. Turning
//! this into a real lib (one compiled copy, its own feature plumbing) would change codegen for
//! every family's firmware at once. This stub lib exists so the workspace owns the crate identity
//! and the docs have a home.
//!
//! Under `cfg(test)` only, it also compiles several of these files -- the clock folds, the RP2350
//! clock-tree derivation and TRNG driver, and the resident-corlib default -- so their pure
//! functions are tested on the host. Clock arithmetic in particular fails as a clock that merely
//! drifts rather than one that stops, which is worth catching without a board. Nothing is compiled
//! into a firmware by this: the binaries still `#[path]`-include the files.

#[cfg(test)]
#[path = "systick_clock.rs"]
mod systick_clock;

#[cfg(test)]
#[path = "systimer_clock.rs"]
mod systimer_clock;

#[cfg(test)]
#[path = "timer0_clock.rs"]
mod timer0_clock;

#[cfg(test)]
#[path = "rp2350_trng.rs"]
mod rp2350_trng;
#[cfg(test)]
#[path = "rp2350_clocks.rs"]
mod rp2350_clocks;

#[cfg(test)]
#[path = "tls_clock.rs"]
mod tls_clock;

#[cfg(test)]
#[path = "resident_corlib.rs"]
mod resident_corlib;

#[cfg(test)]
use systick_clock::idle_block;

#[cfg(test)]
mod tests {
    use super::systick_clock::{elapsed, fold, micros, verdict, ClockSource};

    #[test]
    fn microseconds_come_from_the_same_fold_and_only_ever_grow() {
        let ticks_per_ms = 150_000;
        assert_eq!(micros(0, 0, ticks_per_ms), 0);
        assert_eq!(micros(0, 149, ticks_per_ms), 0, "under one microsecond rounds down");
        assert_eq!(micros(0, 150, ticks_per_ms), 1);
        assert_eq!(micros(7, 149_999, ticks_per_ms), 7_999);
        let (mut millis, mut spare, mut last) = (0u64, 0u32, 0u64);
        for _ in 0..(3 * ticks_per_ms + 7) {
            let (whole, rest) = fold(1, ticks_per_ms, spare);
            millis += u64::from(whole);
            spare = rest;
            let now = micros(millis, spare, ticks_per_ms);
            assert!(now >= last, "went back from {last} to {now}");
            last = now;
        }
        assert_eq!(last, 3_000);
    }
    use super::timer0_clock;

    /// The boot check's decision, separated from the register reads so it can be proved on a host
    /// with no SysTick. The three verdicts are three different remedies, which is why they are three
    /// values and not a bool.
    #[test]
    fn a_clock_that_does_not_move_is_told_apart_from_one_with_no_range_to_move_over() {
        assert_eq!(verdict(0x00ff_ffff, true), ClockSource::Running);
        assert_eq!(verdict(0x00ff_ffff, false), ClockSource::Stopped);
        assert_eq!(verdict(0, false), ClockSource::NoReload);
        assert_eq!(verdict(0, true), ClockSource::NoReload);
    }

    #[test]
    fn a_down_counter_that_did_not_wrap_is_the_plain_difference() {
        assert_eq!(elapsed(1_000, 400, 0x00ff_ffff), 600);
        assert_eq!(elapsed(400, 400, 0x00ff_ffff), 0);
    }

    #[test]
    fn a_counter_that_passed_zero_counts_through_the_reload() {
        assert_eq!(elapsed(10, 995, 999), 15);
        assert_eq!(elapsed(999, 0, 999), 999);
        assert_eq!(elapsed(0, 999, 999), 1);
    }

    #[test]
    fn sub_millisecond_reads_accumulate_instead_of_rounding_away() {
        let ticks_per_ms = 1_000;
        let mut spare = 0;
        let mut millis: u64 = 0;
        for _ in 0..10_000 {
            let (whole, rest) = fold(1, ticks_per_ms, spare);
            millis += u64::from(whole);
            spare = rest;
        }
        assert_eq!(millis, 10, "10,000 single ticks at 1,000/ms is 10 ms, however finely it is read");
    }

    /// The loss this arithmetic cannot see, asserted so the limit is a test and not a comment.
    ///
    /// `elapsed` resolves at most one wrap. Past that it does not report an error, does not
    /// saturate, and does not look wrong -- it returns a plausible small number, because a
    /// down-counter read twice cannot tell one wrap from fifty.
    ///
    /// And a wrong answer here is indistinguishable from a right one, which is why the limit is
    /// asserted rather than described. At 16 MHz, a 50-second interval bracketed by two `DateTime`
    /// reads loses about 48 wraps and reports a sub-second remainder such as 135 ms: stable across
    /// repetitions, linear in the iteration count, and consistent with a checksum over the work it
    /// timed. Wrong by a factor of hundreds, with every signal a caller would think to check saying
    /// it is fine.
    ///
    /// So the invariant is enforced elsewhere -- the interpreter pumps the fold at every quantum
    /// (`lamella-cil-runtime`'s `TIME_SLICE_QUANTUM`: 256 instructions against a wrap of about
    /// 1 s) -- and this test exists to state why that pump may not be removed.
    #[test]
    fn more_than_one_wrap_between_reads_is_silently_lost_which_is_why_the_fold_must_be_pumped() {
        let reload = 0x00ff_ffff;
        let per_wrap = reload + 1;

        assert_eq!(elapsed(10, 20, reload), 10 + per_wrap - 20);

        let one_wrap = elapsed(10, 20, reload);
        let what_two_wraps_really_elapsed = one_wrap + per_wrap;
        assert_eq!(
            one_wrap, what_two_wraps_really_elapsed - per_wrap,
            "two wraps are indistinguishable from one at the counter"
        );

        let ticks_per_ms = 16_000u32;
        let wrap_ms = u64::from(per_wrap / ticks_per_ms);
        let real_ms =
            u64::from(what_two_wraps_really_elapsed + 46 * per_wrap) / u64::from(ticks_per_ms);
        assert!(real_ms > 50_000, "the modelled span really is ~50 s, got {real_ms}");
        for gap in [one_wrap, per_wrap / 8, per_wrap / 2, per_wrap - 1] {
            let (reported_ms, _) = fold(gap, ticks_per_ms, 0);
            assert!(
                u64::from(reported_ms) <= wrap_ms,
                "no reading may exceed one wrap ({wrap_ms} ms); got {reported_ms}"
            );
            assert!(
                u64::from(reported_ms) < real_ms / 40,
                "and against a ~50 s span every one of them is an under-report"
            );
        }

        let step = per_wrap * 2 / 5;
        let mut total = 0u64;
        let mut position = reload;
        for _ in 0..10 {
            let next = (position + per_wrap - step) % per_wrap;
            total += u64::from(elapsed(position, next, reload));
            position = next;
        }
        assert_eq!(
            total,
            u64::from(step) * 10,
            "a bounded read cadence loses nothing, which is exactly what the quantum pump enforces"
        );
    }

    #[test]
    fn one_big_read_and_many_small_ones_agree() {
        let ticks_per_ms = 168_000;
        let (bulk, _) = fold(7 * ticks_per_ms + 12_345, ticks_per_ms, 0);
        let mut spare = 0;
        let mut piecemeal = 0;
        for _ in 0..(7 * ticks_per_ms + 12_345) {
            let (whole, rest) = fold(1, ticks_per_ms, spare);
            piecemeal += whole;
            spare = rest;
        }
        assert_eq!(bulk, 7);
        assert_eq!(piecemeal, bulk);
    }
}

/// The lost-wakeup race, shown by running the unsafe order beside the safe one.
///
/// The block point's correctness is not in what it computes -- it is in when each of four
/// instructions runs relative to an interrupt, and no assertion about the code's output can see
/// that. So the core is modeled instead, in just enough detail to exhibit the failure: an interrupt
/// that arrives while interrupts are enabled runs its handler and leaves; one that arrives while
/// they are masked cannot run and becomes pending instead; and `WFI` returns immediately if
/// something is pending and otherwise stops the core.
///
/// Those three lines are the whole of the hardware behavior the idiom depends on, and they are the
/// architecture's (ARMv7-M B1.5.17 / ARMv6-M B1.5.16), not a convenience of the model.
///
/// The two orderings are then run against the same interrupt at the same instant, one variable
/// between them. One board hangs and the other does not.
#[cfg(test)]
mod idle_block_race {
    use super::idle_block::{Decision, SleepMode};

    /// A Cortex-M core, modeled only where this race lives.
    #[derive(Default, Debug)]
    struct Core {
        /// PRIMASK. While set, an arriving interrupt cannot be taken -- but it still latches.
        masked: bool,
        /// An interrupt has arrived and not yet been taken.
        pending: bool,
        /// A handler ran and left something for the scheduler (a byte in the carrier ring).
        work: bool,
        /// The core executed `WFI` with nothing pending. Nothing else is coming: this is the hang.
        stopped_forever: bool,
    }

    impl Core {
        /// One interrupt arrives. Masked, it latches; unmasked, its handler runs to completion and
        /// the interrupt is no longer pending -- which is precisely what makes the naive order
        /// unsafe, because the evidence moves from the hardware into software.
        fn irq(&mut self) {
            if self.masked {
                self.pending = true;
            } else {
                self.work = true;
            }
        }

        /// `WFI`: wakes on a pending interrupt regardless of PRIMASK; otherwise stops the core.
        fn wfi(&mut self) {
            if self.pending {
                self.pending = false;
            } else {
                self.stopped_forever = true;
            }
        }
    }

    /// The unsafe order: ask whether there is work, then mask. `irq_at_the_window` is the interrupt
    /// landing in the gap between the two.
    fn check_then_mask(core: &mut Core, irq_at_the_window: bool) {
        let work = core.work;
        if irq_at_the_window {
            core.irq();
        }
        core.masked = true;
        if let Decision::Sleep(_) = Decision::for_idle(work, true, true) {
            core.wfi();
        }
        core.masked = false;
    }

    /// The safe order: mask, then ask. Same interrupt, same instant.
    fn mask_then_check(core: &mut Core, irq_at_the_window: bool) {
        core.masked = true;
        if irq_at_the_window {
            core.irq();
        }
        let work = core.work;
        if let Decision::Sleep(_) = Decision::for_idle(work, true, true) {
            core.wfi();
        }
        core.masked = false;
    }

    /// The proof: one interrupt, one instant, two orders, opposite outcomes.
    #[test]
    fn checking_before_masking_sleeps_through_an_interrupt_and_hangs_the_board() {
        let mut naive = Core::default();
        check_then_mask(&mut naive, true);
        assert!(
            naive.stopped_forever,
            "the naive order must exhibit the hang, or this test is proving nothing: {naive:?}"
        );
        assert!(naive.work, "the handler ran -- there is work waiting");
        assert!(!naive.pending, "and nothing is pending, so no WFI will ever return");

        let mut fixed = Core::default();
        mask_then_check(&mut fixed, true);
        assert!(!fixed.stopped_forever, "masking first must not sleep through it: {fixed:?}");
        assert!(!fixed.work, "the handler never ran; it was masked");
        assert!(!fixed.pending, "and the WFI consumed the pending bit rather than stopping");
    }

    /// The control that keeps the proof above honest: with no interrupt in the window, both orders
    /// behave identically and both stop the core. So the difference measured above is the race and
    /// not some other asymmetry between the two functions.
    #[test]
    fn without_the_interrupt_both_orders_agree_so_the_race_is_what_separates_them() {
        let mut naive = Core::default();
        check_then_mask(&mut naive, false);
        let mut fixed = Core::default();
        mask_then_check(&mut fixed, false);
        assert!(naive.stopped_forever && fixed.stopped_forever);
        assert_eq!(naive.work, fixed.work);
        assert_eq!(naive.pending, fixed.pending);
    }

    /// The other half of the window, and the reason the re-check is not redundant with the hardware:
    /// an interrupt that arrived and was serviced before the block point began leaves no pending bit
    /// at all. Only the software re-check can see it, and without one the core would sleep on top of
    /// work that is already in hand.
    #[test]
    fn a_handler_that_ran_before_the_block_point_is_caught_by_the_recheck_alone() {
        let mut core = Core::default();
        core.irq();
        assert!(core.work && !core.pending, "no hardware evidence survives -- only the ring");

        mask_then_check(&mut core, false);
        assert!(!core.stopped_forever, "the re-check must find it: nothing pending would wake us");
    }

    /// The decision table itself. `Spin` is the safety arm and it is asserted as such: a board with
    /// no timer wake source must not sleep on a deadline, because it has no way to end the sleep.
    #[test]
    fn a_board_that_cannot_be_woken_at_a_deadline_refuses_to_sleep_through_one() {
        assert_eq!(Decision::for_idle(true, true, true), Decision::Runnable);
        assert_eq!(Decision::for_idle(true, false, false), Decision::Runnable);
        assert_eq!(Decision::for_idle(false, true, true), Decision::Sleep(SleepMode::CoreOnly));
        assert_eq!(Decision::for_idle(false, false, true), Decision::Spin);
        assert_eq!(Decision::for_idle(false, false, false), Decision::Sleep(SleepMode::CoreOnly));
    }

    /// An unregistered board never sleeps. The default has to fail safe, because otherwise every
    /// board that had not opted in would be one interrupt away from the hang above.
    #[test]
    fn a_board_that_has_not_said_how_it_would_know_is_assumed_to_have_work() {
        assert!(
            super::idle_block::work_pending(),
            "with no predicate registered the answer must be the conservative one"
        );
        assert_eq!(super::idle_block::block(1_000, true), Decision::Runnable);
    }
}

#[cfg(test)]
mod systimer_tests {
    use super::systimer_clock::{counter_from, ticks_to_ms, COUNTER_BITS};

    /// The 52-bit count is assembled from a 20-bit high half above a 32-bit low half. The case that
    /// matters is the one a short test never reaches: a count past 2^32, where a wrong shift stops
    /// being invisible. At 16 MHz that is about four and a half minutes of uptime.
    #[test]
    fn the_two_latched_halves_assemble_high_above_low() {
        assert_eq!(counter_from(0, 0), 0);
        assert_eq!(counter_from(0, 0xFFFF_FFFF), 0xFFFF_FFFF);
        assert_eq!(counter_from(1, 0), 0x1_0000_0000);
        assert_eq!(counter_from(0x000F_FFFF, 0xFFFF_FFFF), (1u64 << COUNTER_BITS) - 1);
    }

    /// 16,000 ticks per millisecond, from the documented 1/16 us per tick.
    #[test]
    fn ticks_convert_at_sixteen_thousand_per_millisecond() {
        assert_eq!(ticks_to_ms(0), 0);
        assert_eq!(ticks_to_ms(15_999), 0, "a sub-millisecond count is zero whole milliseconds");
        assert_eq!(ticks_to_ms(16_000), 1);
        assert_eq!(ticks_to_ms(3_000 * 16_000), 3_000, "the Thread.Sleep(3000) the bench checks");
    }

    /// The property the SysTick fold needs a running remainder for, which this does not need: the
    /// same instant must read the same however often it is read, because every reading is an
    /// absolute division of one free-running counter rather than a sum of differences. Read the same
    /// tick count a thousand times and the answer cannot drift.
    #[test]
    fn repeated_reads_of_the_same_instant_do_not_accumulate_error() {
        let ticks = 7 * 16_000 + 12_345;
        let once = ticks_to_ms(ticks);
        for _ in 0..1_000 {
            assert_eq!(ticks_to_ms(ticks), once);
        }
        assert_eq!(once, 7);
    }

    /// Why no wrap handling exists: the counter's full range is years, not minutes. Asserted rather
    /// than claimed in a comment, so a future narrowing of the counter breaks a test instead of
    /// quietly introducing a wrap nothing resolves.
    #[test]
    fn the_counter_does_not_wrap_within_any_plausible_uptime() {
        let full_range_ms = ticks_to_ms((1u64 << COUNTER_BITS) - 1);
        let years = full_range_ms / (1000 * 60 * 60 * 24 * 365);
        assert!(years >= 8, "a 52-bit counter at 16 MHz should span years, got {years}");
    }
}

/// The RP2350 clock-tree derivation, tested against register words read from a real Pico 2 rather
/// than composed to suit the code.
#[cfg(test)]
mod rp2350_clock_tree {
    use super::rp2350_clocks::{
        clk_ref_hz_from, clk_sys_hz_from, clk_sys_source_name, divide_clk_ref, divide_clk_sys,
        pll_hz, ClockTree, Pll,
    };

    /// The Pico 2's crystal, and the only external fact this derivation takes on trust. It is a
    /// component on the board, so it is a board fact rather than a claim about software.
    const XOSC_HZ: u32 = 12_000_000;

    /// The clock tree of a Pico 2 running the non-`usb` firmware build, word for word as read from
    /// the board.
    ///
    /// The firmware never programs any of this: it is what the boot ROM left. `SYST_RVR` read back
    /// as `0x00ff_ffff` at the same time, which is the non-`usb` build's reload and not the `usb`
    /// build's, so these words belong to the build that leaves the clock tree alone.
    fn pico2_as_the_bootrom_left_it() -> ClockTree {
        ClockTree {
            clk_ref_ctrl: 0x0000_0001,
            clk_ref_div: 0x0002_0000,
            clk_sys_ctrl: 0x0000_0021,
            clk_sys_div: 0x0001_0000,
            pll_sys: Pll { cs: 0x0000_0001, pwr: 0x0000_002d, fbdiv_int: 0, prim: 0x0007_7000 },
            pll_usb: Pll { cs: 0x8000_0001, pwr: 0x0000_0000, fbdiv_int: 100, prim: 0x0005_5000 },
        }
    }

    /// What `clocks_init` leaves on the `usb` build: clk_sys on a locked PLL_SYS at 150 MHz.
    fn pico2_after_clocks_init() -> ClockTree {
        ClockTree {
            clk_ref_ctrl: 0x0000_0002, // SRC = XOSC
            clk_ref_div: 0x0001_0000,
            clk_sys_ctrl: 0x0000_0001, // SRC = aux, AUXSRC = PLL_SYS
            clk_sys_div: 0x0001_0000,
            pll_sys: Pll { cs: 0x8000_0001, pwr: 0, fbdiv_int: 125, prim: 0x0005_2000 },
            pll_usb: Pll { cs: 0x8000_0001, pwr: 0, fbdiv_int: 100, prim: 0x0005_5000 },
        }
    }

    /// As the boot ROM left this board, `clk_sys` runs at 48 MHz, four times the crystal: a firmware
    /// that assumed the crystal's 12 MHz would report every duration four times too long.
    #[test]
    fn the_board_that_reported_every_duration_four_times_too_long_is_running_at_48_mhz() {
        let tree = pico2_as_the_bootrom_left_it();
        assert_eq!(clk_sys_hz_from(&tree, XOSC_HZ), Some(48_000_000));
        assert_eq!(clk_sys_source_name(&tree), "pll_usb");
        assert_eq!(clk_sys_hz_from(&tree, XOSC_HZ).unwrap() / 12_000_000, 4);
    }

    /// A second, independent witness to the same 48 MHz, and it is why the answer is believed
    /// rather than merely computed: the boot ROM also programmed `TICKS.TIMER0_CYCLES` to 24 to
    /// produce a 1 us tick from `clk_ref`. That is the boot ROM stating its own belief about
    /// `clk_ref` in a register, and it agrees -- clk_ref is clk_sys through a divide-by-2.
    #[test]
    fn the_bootrom_recorded_its_own_belief_about_clk_ref_and_it_agrees() {
        let tree = pico2_as_the_bootrom_left_it();
        assert_eq!(clk_ref_hz_from(&tree, XOSC_HZ), Some(24_000_000));
        const TIMER0_CYCLES_READ_FROM_THE_BOARD: u32 = 24;
        assert_eq!(
            clk_ref_hz_from(&tree, XOSC_HZ).unwrap() / TIMER0_CYCLES_READ_FROM_THE_BOARD,
            1_000_000,
            "the tick generator's divisor should yield exactly 1 MHz from the derived clk_ref"
        );
    }

    /// The control that makes the row above mean something: the same derivation over the other
    /// build's tree must give a different answer. One number that happens to be right is not
    /// evidence that any word was read.
    #[test]
    fn the_usb_build_derives_its_own_rate_and_it_is_not_the_other_one() {
        assert_eq!(clk_sys_hz_from(&pico2_after_clocks_init(), XOSC_HZ), Some(150_000_000));
        assert_eq!(clk_sys_source_name(&pico2_after_clocks_init()), "pll_sys");
        assert_ne!(
            clk_sys_hz_from(&pico2_after_clocks_init(), XOSC_HZ),
            clk_sys_hz_from(&pico2_as_the_bootrom_left_it(), XOSC_HZ)
        );
    }

    /// The refusal, and it is the chip's reset selection rather than a defensive branch: SRC = aux
    /// with AUXSRC = 2 is the ROSC, a randomized ring oscillator the datasheet only bounds to a
    /// range. There is no number to return, so nothing is returned.
    #[test]
    fn a_clk_sys_on_the_ring_oscillator_is_refused_rather_than_given_a_number() {
        let mut tree = pico2_as_the_bootrom_left_it();
        tree.clk_sys_ctrl = (2 << 5) | 1;
        assert_eq!(clk_sys_hz_from(&tree, XOSC_HZ), None);
        assert_eq!(clk_sys_source_name(&tree), "rosc");
        let mut through_ref = pico2_as_the_bootrom_left_it();
        through_ref.clk_sys_ctrl = 0;
        through_ref.clk_ref_ctrl = 0;
        assert_eq!(clk_sys_hz_from(&through_ref, XOSC_HZ), None);
        assert_eq!(clk_sys_source_name(&through_ref), "clk_ref/rosc");
    }

    /// A PLL that is not running is not a rate. The board's own PLL_SYS is in exactly this state,
    /// so this is a real capture and not an invented one -- and note it would otherwise compute
    /// something, since `fbdiv_int` reads 0 and the arithmetic would answer 0 Hz.
    #[test]
    fn a_powered_down_or_unlocked_pll_yields_nothing_rather_than_zero() {
        let board = pico2_as_the_bootrom_left_it();
        assert_eq!(pll_hz(&board.pll_sys, XOSC_HZ), None);
        let running = board.pll_usb;
        assert_eq!(pll_hz(&running, XOSC_HZ), Some(48_000_000));
        assert_eq!(pll_hz(&Pll { cs: running.cs & !(1 << 31), ..running }, XOSC_HZ), None, "lock");
        assert_eq!(pll_hz(&Pll { cs: running.cs | (1 << 8), ..running }, XOSC_HZ), None, "bypass");
        assert_eq!(pll_hz(&Pll { pwr: 1 << 0, ..running }, XOSC_HZ), None, "pd");
        assert_eq!(pll_hz(&Pll { pwr: 1 << 3, ..running }, XOSC_HZ), None, "postdivpd");
        assert_eq!(pll_hz(&Pll { pwr: 1 << 5, ..running }, XOSC_HZ), None, "vcopd");
        assert_eq!(pll_hz(&Pll { pwr: 1 << 2, ..running }, XOSC_HZ), Some(48_000_000), "dsmpd");
    }

    /// The two divider registers do not share a layout, and a word that reads plausibly under the
    /// wrong one is how that goes unnoticed. `CLK_REF_DIV.INT` is bits 23:16 with no fractional
    /// part; `CLK_SYS_DIV` is 31:16 integer over a 16-bit fraction.
    #[test]
    fn the_two_divider_registers_are_read_with_their_own_layouts() {
        assert_eq!(divide_clk_ref(48_000_000, 0x0302_0000), Some(24_000_000));
        assert_eq!(divide_clk_ref(48_000_000, 0x0000_0000), Some(187_500), "INT 0 means 256");
        assert_eq!(divide_clk_sys(48_000_000, 0x0001_8000), Some(32_000_000));
        assert_eq!(divide_clk_sys(48_000_000, 0x0001_0000), Some(48_000_000));
        assert_eq!(divide_clk_sys(48_000_000, 0x0000_0000), Some(732));
    }

    /// The crystal is the only input, so a board with a different one derives a different rate
    /// through the same words. This is what makes the function a derivation rather than a lookup.
    #[test]
    fn the_rate_follows_the_crystal_it_is_derived_from() {
        assert_eq!(clk_sys_hz_from(&pico2_as_the_bootrom_left_it(), 6_000_000), Some(24_000_000));
    }
}

#[cfg(test)]
mod timer0_clock_tests {
    use super::timer0_clock::{elapsed, running_from};

    #[test]
    fn elapsed_is_the_plain_difference_while_the_counter_climbs() {
        assert_eq!(elapsed(0, 0), 0);
        assert_eq!(elapsed(10, 25), 15);
        assert_eq!(elapsed(0, u32::MAX), u32::MAX);
    }

    #[test]
    fn elapsed_resolves_exactly_one_wrap() {
        assert_eq!(elapsed(u32::MAX, 1), 2);
        assert_eq!(elapsed(u32::MAX - 9, 0), 10);
        assert_eq!(elapsed(1_234, 1_234), 0);
    }

    #[test]
    fn a_counter_that_never_moved_is_not_running() {
        assert!(!running_from(false));
        assert!(running_from(true));
    }
}
