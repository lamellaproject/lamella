//! The trace: the attach recorded through the wrapper wire decodes back to
//! the script's rows, and the decoded rows replay to the same outcome.

use crate::driver::{Driver, Outcome};
use crate::fixture::{FakeClock, FakeWire, Row, rows_of_trace, run};
use crate::gspi::{Gspi, GspiWire};
use crate::trace::{TraceSink, Traced, tag};
use std::boxed::Box;
use std::vec::Vec;

struct VecSink(Vec<u8>);

impl TraceSink for VecSink {
    fn write(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
    }
}

fn same_row(a: &Row, b: &Row) -> bool {
    match (a, b) {
        (Row::Power(x), Row::Power(y))
        | (Row::Strap(x), Row::Strap(y))
        | (Row::Irq(x), Row::Irq(y)) => x == y,
        (Row::Frame { tx: ta, rx: ra, .. }, Row::Frame { tx: tb, rx: rb, .. }) => {
            ta == tb && ra == rb
        }
        _ => false,
    }
}

/// The expected length of the attach's trace: two power records, two
/// strap records, and a frame record per script frame.
fn expected_len(script: &[Row]) -> usize {
    script
        .iter()
        .map(|row| match row {
            Row::Frame { tx, rx, .. } => 3 + tx.len() + 2 + rx.len(),
            _ => 2,
        })
        .sum()
}

fn traced_attach() -> &'static [u8] {
    let script = super::attach_gspi::attach_script();
    let mut bus = Gspi::new(Traced::new(FakeWire::new(&script), VecSink(Vec::new())));
    let mut clock = FakeClock::new(0);
    let mut driver = Driver::new();
    assert!(driver.attach());
    let record = run(&mut driver, &mut bus, &mut clock, 10_000);
    assert_eq!(
        record.outcome,
        Some(Outcome::Attached {
            chip_id: 0x1545_A9AF
        })
    );
    let (wire, sink) = bus.into_wire().into_parts();
    assert_eq!(wire.remaining(), 0);
    assert_eq!(sink.0.len(), expected_len(&script));
    Box::leak(sink.0.into_boxed_slice())
}

#[test]
fn the_traced_attach_decodes_to_the_script_and_replays() {
    let trace = traced_attach();
    let script = super::attach_gspi::attach_script();
    let mut rows = [Row::Irq(false); 32];
    let n = rows_of_trace(trace, &mut rows).expect("a whole trace");
    assert_eq!(n, script.len());
    for (i, (a, b)) in rows[..n].iter().zip(script.iter()).enumerate() {
        assert!(same_row(a, b), "row {}", i + 1);
    }

    let mut bus = Gspi::new(FakeWire::new(&rows[..n]));
    let mut clock = FakeClock::new(0);
    let mut driver = Driver::new();
    assert!(driver.attach());
    let record = run(&mut driver, &mut bus, &mut clock, 10_000);
    assert_eq!(
        record.outcome,
        Some(Outcome::Attached {
            chip_id: 0x1545_A9AF
        })
    );
    assert_eq!(record.polls, 12);
    assert_eq!(record.deadlines(), &[50_000, 100_000, 110_000, 210_000]);
    assert_eq!(bus.wire().remaining(), 0);
}

#[test]
fn a_truncated_trace_is_refused_at_its_record() {
    let trace = traced_attach();
    let cut = &trace[..trace.len() - 1];
    let mut rows = [Row::Irq(false); 32];
    let last_frame = trace.len() - (3 + 5 + 2);
    assert_eq!(trace[last_frame], tag::FRAME);
    assert_eq!(rows_of_trace(cut, &mut rows), Err(last_frame));
}

#[test]
fn an_unknown_tag_is_refused_at_its_offset() {
    static BAD: [u8; 5] = [tag::POWER, 1, 0x7F, 0, 0];
    let mut rows = [Row::Irq(false); 4];
    assert_eq!(rows_of_trace(&BAD, &mut rows), Err(2));
}

#[test]
fn a_select_record_makes_no_row_and_a_full_buffer_refuses() {
    static TRACE: [u8; 9] = [
        tag::SELECT,
        2,
        0xFF,
        tag::IRQ,
        1,
        tag::STRAP,
        0,
        tag::POWER,
        1,
    ];
    let mut rows = [Row::Irq(false); 3];
    assert_eq!(rows_of_trace(&TRACE, &mut rows), Ok(3));
    assert!(same_row(&rows[0], &Row::Irq(true)));
    assert!(same_row(&rows[1], &Row::Strap(false)));
    assert!(same_row(&rows[2], &Row::Power(true)));
    let mut two = [Row::Irq(false); 2];
    assert_eq!(rows_of_trace(&TRACE, &mut two), Err(7));
}

#[test]
fn the_wrapper_passes_the_settings_through_and_records_a_selection() {
    let script = super::attach_gspi::attach_script();
    let mut wire = Traced::new(FakeWire::new(&script), VecSink(Vec::new()));
    assert_eq!(wire.settings(), 0);
    assert_eq!(wire.selected(), (0, 0));
    wire.select(1, -2);
    assert!(!wire.irq_asserted(), "the script has no idle level to give");
    let (_, sink) = wire.into_parts();
    assert_eq!(sink.0, [tag::SELECT, 1, 0xFE, tag::IRQ, 0]);
}
