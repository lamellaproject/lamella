//! The trace decoder: a recorded trace turned back into the wire rows the
//! fake wire replays, borrowing the trace's own bytes.

use super::wire::Row;
use crate::trace::tag;

/// The rows of a trace, written into `rows`: the count, or the offset of a
/// record that could not be decoded (a truncated record, an unknown tag,
/// or a row past the end of `rows`). A setting selected makes no row.
pub fn rows_of_trace(trace: &'static [u8], rows: &mut [Row]) -> Result<usize, usize> {
    let mut at = 0usize;
    let mut n = 0usize;
    while at < trace.len() {
        let start = at;
        let row = match trace[at] {
            tag::POWER | tag::STRAP | tag::IRQ => {
                let Some(&arg) = trace.get(at + 1) else {
                    return Err(start);
                };
                at += 2;
                match trace[start] {
                    tag::POWER => Row::Power(arg != 0),
                    tag::STRAP => Row::Strap(arg != 0),
                    _ => Row::Irq(arg != 0),
                }
            }
            tag::FRAME => {
                let Some(out) = length(trace, at + 1) else {
                    return Err(start);
                };
                let tx_start = at + 3;
                let tx_end = tx_start + out;
                let Some(back) = length(trace, tx_end) else {
                    return Err(start);
                };
                let rx_start = tx_end + 2;
                let rx_end = rx_start + back;
                if rx_end > trace.len() || out < 4 {
                    return Err(start);
                }
                at = rx_end;
                Row::Frame {
                    tx: &trace[tx_start..tx_end],
                    rx: &trace[rx_start..rx_end],
                    times: 1,
                    note: "trace",
                }
            }
            tag::SELECT => {
                if at + 3 > trace.len() {
                    return Err(start);
                }
                at += 3;
                continue;
            }
            _ => return Err(start),
        };
        if n >= rows.len() {
            return Err(start);
        }
        rows[n] = row;
        n += 1;
    }
    Ok(n)
}

fn length(trace: &[u8], at: usize) -> Option<usize> {
    let lo = *trace.get(at)?;
    let hi = *trace.get(at + 1)?;
    Some(usize::from(u16::from_le_bytes([lo, hi])))
}
