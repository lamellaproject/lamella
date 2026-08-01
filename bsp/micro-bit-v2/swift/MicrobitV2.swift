// HAND-WRITTEN thin board class for the BBC micro:bit v2 in Swift: constructs pin descriptors
// from the GENERATED MicroBitV2Bindings constants and exposes the board's display. Nothing
// here is a hardware literal -- every address, mask and polarity is read from the generated
// board bindings, which in turn come from bsp/microbit-v2/board.toml.
//
// Embedded-Swift subset: no heap. Pin descriptors are produced on demand by `switch` over the
// generated constants rather than stored in an array, so the whole display driver runs on the
// stack with no allocator present.

/// The board's 5x5 LED display.
///
/// The matrix is SCANNED, not statically driven: only one row is energized at a time, and the
/// eye integrates a fast enough sweep into a steady image. Row lines SOURCE current (asserted
/// high) and column lines SINK it (asserted low), so a pixel lights when BOTH its row and its
/// column are asserted -- the polarity difference is a board fact, and it arrives here as each
/// pin's generated `ACTIVE_LOW` constant rather than as an assumption in this code.
///
/// A frame is a 25-bit map, bit `row * 5 + column`, origin top-left.
public struct MicroBitV2Display {
    private let gpio: Nrf52833Gpio

    public init(_ gpio: Nrf52833Gpio) {
        self.gpio = gpio
    }

    /// The five row lines, in display order.
    static func row(_ i: Int) -> Nrf52833PinBinding {
        switch i {
        case 0: return Nrf52833PinBinding(portBase: MicroBitV2Bindings.DISPLAY_ROW1_PORT_BASE,
                                          mask: MicroBitV2Bindings.DISPLAY_ROW1_MASK,
                                          activeLow: MicroBitV2Bindings.DISPLAY_ROW1_ACTIVE_LOW == 1)
        case 1: return Nrf52833PinBinding(portBase: MicroBitV2Bindings.DISPLAY_ROW2_PORT_BASE,
                                          mask: MicroBitV2Bindings.DISPLAY_ROW2_MASK,
                                          activeLow: MicroBitV2Bindings.DISPLAY_ROW2_ACTIVE_LOW == 1)
        case 2: return Nrf52833PinBinding(portBase: MicroBitV2Bindings.DISPLAY_ROW3_PORT_BASE,
                                          mask: MicroBitV2Bindings.DISPLAY_ROW3_MASK,
                                          activeLow: MicroBitV2Bindings.DISPLAY_ROW3_ACTIVE_LOW == 1)
        case 3: return Nrf52833PinBinding(portBase: MicroBitV2Bindings.DISPLAY_ROW4_PORT_BASE,
                                          mask: MicroBitV2Bindings.DISPLAY_ROW4_MASK,
                                          activeLow: MicroBitV2Bindings.DISPLAY_ROW4_ACTIVE_LOW == 1)
        default: return Nrf52833PinBinding(portBase: MicroBitV2Bindings.DISPLAY_ROW5_PORT_BASE,
                                           mask: MicroBitV2Bindings.DISPLAY_ROW5_MASK,
                                           activeLow: MicroBitV2Bindings.DISPLAY_ROW5_ACTIVE_LOW == 1)
        }
    }

    /// The five column lines, in display order. Column 4 is the one that lives on the chip's
    /// SECOND GPIO port -- which is why a column is addressed by (port base, mask) and never by
    /// a flattened pin index.
    static func column(_ i: Int) -> Nrf52833PinBinding {
        switch i {
        case 0: return Nrf52833PinBinding(portBase: MicroBitV2Bindings.DISPLAY_COL1_PORT_BASE,
                                          mask: MicroBitV2Bindings.DISPLAY_COL1_MASK,
                                          activeLow: MicroBitV2Bindings.DISPLAY_COL1_ACTIVE_LOW == 1)
        case 1: return Nrf52833PinBinding(portBase: MicroBitV2Bindings.DISPLAY_COL2_PORT_BASE,
                                          mask: MicroBitV2Bindings.DISPLAY_COL2_MASK,
                                          activeLow: MicroBitV2Bindings.DISPLAY_COL2_ACTIVE_LOW == 1)
        case 2: return Nrf52833PinBinding(portBase: MicroBitV2Bindings.DISPLAY_COL3_PORT_BASE,
                                          mask: MicroBitV2Bindings.DISPLAY_COL3_MASK,
                                          activeLow: MicroBitV2Bindings.DISPLAY_COL3_ACTIVE_LOW == 1)
        case 3: return Nrf52833PinBinding(portBase: MicroBitV2Bindings.DISPLAY_COL4_PORT_BASE,
                                          mask: MicroBitV2Bindings.DISPLAY_COL4_MASK,
                                          activeLow: MicroBitV2Bindings.DISPLAY_COL4_ACTIVE_LOW == 1)
        default: return Nrf52833PinBinding(portBase: MicroBitV2Bindings.DISPLAY_COL5_PORT_BASE,
                                           mask: MicroBitV2Bindings.DISPLAY_COL5_MASK,
                                           activeLow: MicroBitV2Bindings.DISPLAY_COL5_ACTIVE_LOW == 1)
        }
    }

    /// Claims all ten lines as outputs, display dark.
    public func initialize() {
        for i in 0 ..< 5 {
            gpio.configureOutput(MicroBitV2Display.row(i))
            gpio.configureOutput(MicroBitV2Display.column(i))
        }
    }

    /// Sweeps the frame once: each row is energized in turn for `holdSpins` of busy-wait, with
    /// exactly that row's lit columns asserted. Call repeatedly to hold an image on screen --
    /// one call is one refresh, not one second.
    public func refresh(_ frame: UInt32, holdSpins: UInt32) {
        for r in 0 ..< 5 {
            let rowPin = MicroBitV2Display.row(r)
            for c in 0 ..< 5 {
                let lit = (frame >> UInt32(r * 5 + c)) & 1 == 1
                gpio.write(MicroBitV2Display.column(c), asserted: lit)
            }
            gpio.assert(rowPin)
            spinDelay(holdSpins)
            gpio.release(rowPin)
        }
    }

    /// Holds one frame on screen for roughly `sweeps` refreshes.
    public func show(_ frame: UInt32, sweeps: UInt32, holdSpins: UInt32) {
        var n: UInt32 = 0
        while n < sweeps {
            n &+= 1
            refresh(frame, holdSpins: holdSpins)
        }
    }
}

/// A crude busy-wait. Reads a generated register address each pass so the loop survives
/// optimization. Not a timebase.
@inline(never)
func spinDelay(_ count: UInt32) {
    var i: UInt32 = 0
    while i < count {
        i &+= 1
        _ = Mmio.read32(MicroBitV2Bindings.DISPLAY_ROW1_PORT_BASE + Nrf52833GpioLayout.IN_OFF)
    }
}
