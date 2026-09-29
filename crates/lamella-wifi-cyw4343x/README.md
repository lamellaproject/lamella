# lamella-wifi-cyw4343x

A `no_std` host driver for the CYW43439 and CYW4343W WiFi radios -- the
parts fitted on the Raspberry Pi Pico W, Pico 2 W and Pimoroni Pico Plus 2 W
boards (the CYW43439 over its serial gSPI interface) and on the Murata Type
1DX module of the Arduino GIGA R1 WiFi and Portenta H7 boards (the CYW4343W
over SDIO). The driver brings the chip up behind one transport trait that a
bus implements, loads the firmware and settings images the caller stages,
and it is pumped by the caller: every wait is a deadline handed back, never
a sleep inside.

- **No sleeps, no timer.** `poll` performs a bounded amount of bus work and
  returns a wake: call again now, call again at an instant, wait for the
  host interrupt or an instant, or an outcome. The caller supplies a
  monotonic microsecond clock.
- **No firmware inside.** The caller supplies the vendor's firmware image
  and board-settings image as byte slices and carries their license
  obligations; the driver writes them into the chip's RAM, one transfer per
  call, reads the firmware back, starts it and waits for the packet channel.
- **The control path opened and proven.** After the upload the caller opens
  the control path with the regulatory blob it stages and the version
  string recorded beside the images: the driver sets the packet channel's
  interrupts up, pushes the blob in chunks, sets the firmware's opening
  configuration and queries its version; the outcome carries the string,
  and a reply that does not contain the expected one is refused by name.
  The packet channel's frame header, its credit window and its abort are
  the driver's, one frame in flight each way through one buffer, at most
  one frame per call.
- **The firmware's events heard.** The opening ends by telling the
  firmware which events to deliver (a default set a join, a scan and the
  link's upkeep need; the caller may add numbers before opening), and once
  ready the driver services the packet channel on every call: each event
  frame is walked to its header (the vendor's ethertype and identifier
  checked, the header's fields read big-endian) and handed to the caller
  as an outcome with its payload readable until the next call; the
  event's reading for the link -- associated, the handshake done, down --
  is a function the crate states once, for the caller's link machine to
  compose.
- **A network joined and kept joined.** Once ready the caller brings the
  radio up and receives the chip's own address, lists the networks in
  range (one record per outcome: the SSID, the BSSID, the channel, the
  signal strength, whether the network is secured, and what it advertises
  about its security -- the authentication suites, the cipher, the
  protection of management frames) and joins one by name with its
  credential: a WPA2 network's passphrase, a WPA3 network's password (the
  chip's firmware runs the SAE exchange and the driver requires protected
  management frames), or none on an open network; the association and
  the usable link are two moments, and a secured join completes only
  once the chip has reported both the handshake done and the association,
  and a short hold after the handshake's word has passed (the chip drops a
  frame sent within a moment of that word). A lost link and a failed
  join are the same situation: the driver returns to a scan and, when
  the network is seen, re-enters the join at its disassociation step
  with the secret re-supplied, without a ceiling, until the caller
  disconnects; the two failures it does not retry are the handshake's
  timeout, the usual sign of a wrong passphrase, and a recovery scan
  that finds the network advertising no suite the credential can use,
  which ends the join with that reason. The firmware's own capability
  string can be read once ready. The credentials are the caller's
  slices, held by reference and never printed.
- **Frames both ways.** While the link is up, every data frame the chip
  delivers is walked to its Ethernet frame and handed to the caller in
  place, and the one Ethernet frame the caller stages in the driver's
  transmit buffer is written under the chip's credit window with a
  sequence number spent only when the chip took it; while the link is not
  up the driver takes no frame and the caller's stack keeps its own. The
  device adapter behind the `smoltcp` feature presents both halves to that
  stack as a network device: the received frame read in place, the
  stack's reply staged and written, an Ethernet medium with a unit of
  1,514 bytes and a burst of one.
- **No dependency beyond `core`** without the `smoltcp` feature (which
  brings that crate at an exact pin), no allocator, no threads. The core
  is pure logic; `unsafe` is denied at the crate root and allowed in the
  two modules that talk to a controller's registers.
- **One trait per bus.** The gSPI transport lives in the crate above a
  four-operation wire trait that a hardware engine or a test fake
  implements; the RP2350's programmable I/O block is one such engine,
  behind the `rp2350-pio` feature, on the Pico 2 W and Pico Plus 2 W pins.
  The SDIO transport lives above a seven-operation host trait that a
  controller or a test fake implements; the STM32H747's SDMMC1 controller
  is one such host, behind the `stm32h7-sdmmc` feature, with the GIGA R1
  WiFi and Portenta H7 pin tables. The bus-agnostic core sees functions,
  addresses and bytes, and the part it expects is a fact of the board.
- **The bus tuned on real traffic.** An engine that can move its sample
  instant and its bit rate offers them as settings; once the chip's RAM is
  writable the gSPI transport measures the data line's eye on 64-byte
  transfers and keeps the fastest setting with 24 ns of margin on both
  sides of the sample instant, proven by a read-back and the test register.
- **Every wire operation recordable.** A wrapper wire writes a trace to
  any byte sink, and the fixtures decode a trace back into the rows the
  fake wire replays, so an exchange with real silicon runs through the
  same tests as a recorded one.

## Building and testing

```
cargo build --target thumbv6m-none-eabi                              # Cortex-M0+ (RP2040)
cargo build --target thumbv8m.main-none-eabihf                       # Cortex-M33 (RP2350)
cargo build --target thumbv8m.main-none-eabihf --features rp2350-pio # with the PIO engine
cargo build --target thumbv7em-none-eabihf --features stm32h7-sdmmc  # Cortex-M7 (STM32H747)
cargo build --target thumbv8m.main-none-eabihf --features smoltcp    # with the device adapter
cargo test --all-features                                            # the host battery
```

The host battery replays recorded bus exchanges through fakes against the
driver, with a fake clock, and asserts the outcome, the wake timeline and
the refusal a doctored exchange must produce: the gSPI attach at the wire,
the SDIO attach at the host, the bus-agnostic core at the transport level
(the attach, the download from upload mode to a running firmware, the
opening of the control path to a ready driver, the events delivered to
it, the radio up, a scan, a join, a lost link recovered, and the data
frames both ways), the device adapter under a `smoltcp` interface, and a
recorder that shows the three levels carrying the same core operations.
The fakes are available to a consumer's own tests behind the `fixture`
feature.

## Vocabulary

- **transport** -- the bus between the host and the chip (gSPI or SDIO),
  behind the `Transport` trait.
- **F0, F1, F2** -- the chip's three bus functions: its own bus registers,
  the window onto its internal address space, and the packet channel.
- **the wire** -- one chip-select frame over gSPI: bytes out, then bytes
  in.
- **the host** -- the SD I/O controller under the SDIO transport: commands,
  responses, data blocks, the bus clock and the card's interrupt.
- **a refusal** -- the driver stopping at a named stage with the status
  word it saw there.

## License

MIT OR Apache-2.0. Copyright (c) Lamella LLC.
