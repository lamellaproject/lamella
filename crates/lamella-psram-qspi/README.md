# lamella-psram-qspi

A `no_std` bring-up for a QSPI PSRAM part behind a microcontroller's
memory-mapped flash interface -- the AP Memory APS6404L family and its
compatible parts, as fitted on the Pimoroni Pico Plus 2 and Pico Plus 2 W
boards (8 MB on the RP2350's second QSPI chip select, routed to GPIO 47).
Such a part shares the flash's clock and four data lines and appears as
ordinary RAM at a fixed address once three things are done: the pin that
carries its chip select is told so, the part is told to speak its four-line
mode through a short command sequence sent by hand, and the interface's
second memory window is told the part's read and write commands and its
timing limits. The crate does those three things and proves the memory by
writing patterns and reading them back.

- **The part is a value.** `Part` holds a part's datasheet facts: its
  commands, the wait cycles of its fast quad read, its burst-rate bound,
  its chip-select limits in nanoseconds and the byte its Read ID answers
  for a good die. `Part::APS6404L_3SQR` is the one part stated today; a
  compatible part from another maker is a second value.
- **The timing is derived, never tuned.** `window_timing` turns a part, a
  system clock and a temperature grade into the window's timing fields:
  the clock divisor under the part's burst rate, the deselect the part needs
  between bursts, the longest assertion the part allows with the worst
  in-flight transfer counted, the read sample delay from the part's output
  delay. A posture the part's limits cannot be met at is refused (the 12
  MHz crystal posture of those boards is one), never approximated. The
  four format and command words are encoded from the part's command table.
- **The proof is a read-back.** `proof::stride_walk` writes one word per
  page and the window's last word through the uncached view and reads them
  back through both views (cheap enough for every boot); `proof::full_walk`
  does every word under two patterns (a gate and a flight record). A wrong
  size, an unwired line, a wrong timing or a window left read-only each
  produce a mismatch naming the address and the two values. The walks are
  written against a small `Memory` trait, so the crate's own tests run them
  over a buffer and over doctored buffers they must refuse.
- **The host is a feature.** `rp2350-qmi` brings the RP2350's QSPI memory
  interface: the pad and function of the chip select's line, the
  direct-mode sequence that wakes the part (the exit-quad nudge for a part
  left in its four-line mode by a warm reset, reset enable and reset, the
  Read ID frame whose known-good-die byte gates the bring-up, enter quad),
  the window's five registers, the write enable, and the cache maintenance
  the proof needs. Direct mode disconnects the flash window from the bus, so
  the sequence lives in one function the crate places in a `.data` section
  (a flash-resident image's map copies it to RAM at boot) that calls
  nothing, reads no constant from flash and bounds every poll.
- **Nothing else inside.** No interrupt masking (the caller runs the
  bring-up before any interrupt source is enabled, or inside its own
  critical section), no sleeps, no prints, no heap: the caller places the
  window in its heap or map as it sees fit.

## Use

```rust
use lamella_psram_qspi::{Grade, Part, proof, rp2350};

let cfg = rp2350::Config {
    select_line: 47,
    part: &Part::APS6404L_3SQR,
    sys_hz: 150_000_000,
    grade: Grade::Standard,
    bytes: 8 * 1024 * 1024,
};
// Before any interrupt source is enabled, with the system clock final.
let window = unsafe { rp2350::bring_up(&cfg) }?;
unsafe { proof::stride_walk(&mut rp2350::Mmio, &window, salt) }?;
// window.base .. window.base + window.bytes is RAM, zero after the walk.
```

## Tests

`cargo test` runs the crate's own tests on the host: the timing words at
the 150 MHz posture and the refusal of the 12 MHz one, the format words,
the walks over a good buffer and over a buffer that aliases like a smaller
part, one that drops a bit and one that ignores writes, and the direct-mode
sequence against a fake register file that answers like the chip -- the
exact records pushed, the window's registers, a failing die refused, a
busy flag that never clears named by its step.

`cargo build --release --target thumbv8m.main-none-eabihf --features rp2350-qmi`
builds the host module for the Cortex-M33.

## License

MIT OR Apache-2.0. Copyright Lamella LLC.
