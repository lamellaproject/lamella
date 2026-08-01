// HAND-WRITTEN thin board class for the BBC micro:bit v1 in Swift: constructs pin descriptors
// from the GENERATED MicroBitV1Bindings constants, and carries the one board fact the strata
// cannot hold -- the charlieplex map.
//
// Embedded-Swift subset: no heap. Descriptors and map entries are produced on demand by `switch`
// over constants, never stored in arrays, so the whole display driver runs on the stack.

/// The board's 5x5 LED display, driven as a CHARLIEPLEXED 3x9 grid.
///
/// This is the part that differs fundamentally from the v2, and the reason its driver could not
/// be shared. On the v2 the grid IS the screen: one pin per row, one per column, and pixel (x,y)
/// is lit by asserting row y and column x. Here 25 LEDs hang off 12 pins arranged as 3 drive
/// lines and 9 sink lines, and which grid position lights which screen pixel follows no
/// arithmetic at all -- it is a wiring table, reproduced below.
///
/// Scanning is therefore per DRIVE LINE, not per screen row: energize drive line r, assert every
/// sink line whose (sink, r) position maps to a pixel that should be lit, hold briefly, release.
/// Three passes cover the whole screen, which is why this display can be refreshed faster than
/// the v2's five.
///
/// A frame is a 25-bit map, bit `y * 5 + x`, origin top-left -- the SAME frame format the v2
/// display takes, so a caller's images are portable across the two boards even though nothing
/// underneath is.
public struct MicroBitV1Display {
    private let gpio: Nrf51Gpio

    public init(_ gpio: Nrf51Gpio) {
        self.gpio = gpio
    }

    /// The three drive (row) lines.
    static func drive(_ i: Int) -> Nrf51PinBinding {
        switch i {
        case 0: return Nrf51PinBinding(portBase: MicroBitV1Bindings.DISPLAY_ROW1_PORT_BASE,
                                       mask: MicroBitV1Bindings.DISPLAY_ROW1_MASK,
                                       activeLow: MicroBitV1Bindings.DISPLAY_ROW1_ACTIVE_LOW == 1)
        case 1: return Nrf51PinBinding(portBase: MicroBitV1Bindings.DISPLAY_ROW2_PORT_BASE,
                                       mask: MicroBitV1Bindings.DISPLAY_ROW2_MASK,
                                       activeLow: MicroBitV1Bindings.DISPLAY_ROW2_ACTIVE_LOW == 1)
        default: return Nrf51PinBinding(portBase: MicroBitV1Bindings.DISPLAY_ROW3_PORT_BASE,
                                        mask: MicroBitV1Bindings.DISPLAY_ROW3_MASK,
                                        activeLow: MicroBitV1Bindings.DISPLAY_ROW3_ACTIVE_LOW == 1)
        }
    }

    /// The nine sink (column) lines.
    static func sink(_ i: Int) -> Nrf51PinBinding {
        switch i {
        case 0: return pin(MicroBitV1Bindings.DISPLAY_COL1_MASK)
        case 1: return pin(MicroBitV1Bindings.DISPLAY_COL2_MASK)
        case 2: return pin(MicroBitV1Bindings.DISPLAY_COL3_MASK)
        case 3: return pin(MicroBitV1Bindings.DISPLAY_COL4_MASK)
        case 4: return pin(MicroBitV1Bindings.DISPLAY_COL5_MASK)
        case 5: return pin(MicroBitV1Bindings.DISPLAY_COL6_MASK)
        case 6: return pin(MicroBitV1Bindings.DISPLAY_COL7_MASK)
        case 7: return pin(MicroBitV1Bindings.DISPLAY_COL8_MASK)
        default: return pin(MicroBitV1Bindings.DISPLAY_COL9_MASK)
        }
    }

    /// Every sink line shares a port and a polarity, so only the mask varies.
    private static func pin(_ mask: UInt32) -> Nrf51PinBinding {
        Nrf51PinBinding(portBase: MicroBitV1Bindings.DISPLAY_COL1_PORT_BASE,
                        mask: mask,
                        activeLow: MicroBitV1Bindings.DISPLAY_COL1_ACTIVE_LOW == 1)
    }

    /// The CHARLIEPLEX MAP: grid position (sink, drive) -> screen pixel, encoded as `y * 5 + x`,
    /// or -1 for the two grid positions with no LED wired to them.
    ///
    /// Transcribed from the micro:bit Foundation's own runtime -- lancaster-university/
    /// microbit-dal, `inc/drivers/MicroBitMatrixMaps.h`, the **MICROBIT_SB2** `microbitDisplayMap`,
    /// which lists 27 {x,y} entries in (column-major, row-minor) order. It is a table, not a
    /// formula: nothing about it can be derived, checked by symmetry, or guessed.
    ///
    /// WHICH TABLE matters as much as reading it correctly. That header defines FOUR display
    /// types, and two of them -- MICROBIT_3X9 and MICROBIT_SB2 -- are both 3 rows by 9 columns
    /// with the same pins and the same counts, differing ONLY in the scramble. The shipping board
    /// is SB2: `MicroBitConfig.h` defaults `MICROBIT_DISPLAY_TYPE` to it. Building against the
    /// other one lights the right number of LEDs in the wrong places, which reads as a working
    /// driver with a corrupt image rather than as a wiring mistake -- and that is exactly how it
    /// presented before this was corrected.
    static func pixel(sink: Int, drive: Int) -> Int {
        // {x, y} pairs exactly as the runtime lists them, flattened here to y * 5 + x.
        switch sink * 3 + drive {
        case 0:  return 0 * 5 + 0   // {0,0}
        case 1:  return 2 * 5 + 4   // {4,2}
        case 2:  return 4 * 5 + 2   // {2,4}
        case 3:  return 0 * 5 + 2   // {2,0}
        case 4:  return 2 * 5 + 0   // {0,2}
        case 5:  return 4 * 5 + 4   // {4,4}
        case 6:  return 0 * 5 + 4   // {4,0}
        case 7:  return 2 * 5 + 2   // {2,2}
        case 8:  return 4 * 5 + 0   // {0,4}
        case 9:  return 3 * 5 + 4   // {4,3}
        case 10: return 0 * 5 + 1   // {1,0}
        case 11: return 1 * 5 + 0   // {0,1}
        case 12: return 3 * 5 + 3   // {3,3}
        case 13: return 0 * 5 + 3   // {3,0}
        case 14: return 1 * 5 + 1   // {1,1}
        case 15: return 3 * 5 + 2   // {2,3}
        case 16: return 4 * 5 + 3   // {3,4}
        case 17: return 1 * 5 + 2   // {2,1}
        case 18: return 3 * 5 + 1   // {1,3}
        case 19: return 4 * 5 + 1   // {1,4}
        case 20: return 1 * 5 + 3   // {3,1}
        case 21: return 3 * 5 + 0   // {0,3}
        case 22: return -1          // NO_CONN
        case 23: return 1 * 5 + 4   // {4,1}
        case 24: return 2 * 5 + 1   // {1,2}
        case 25: return -1          // NO_CONN
        default: return 2 * 5 + 3   // {3,2}
        }
    }

    /// Claims all twelve lines as outputs, display dark.
    public func initialize() {
        for i in 0 ..< 3 { gpio.configureOutput(MicroBitV1Display.drive(i)) }
        for i in 0 ..< 9 { gpio.configureOutput(MicroBitV1Display.sink(i)) }
    }

    /// Sweeps the frame once: each drive line in turn, with exactly the sink lines whose mapped
    /// pixel is lit. One call is one refresh, not one second.
    public func refresh(_ frame: UInt32, holdSpins: UInt32) {
        for d in 0 ..< 3 {
            for s in 0 ..< 9 {
                let pixel = MicroBitV1Display.pixel(sink: s, drive: d)
                let lit = pixel >= 0 && (frame >> UInt32(pixel)) & 1 == 1
                gpio.write(MicroBitV1Display.sink(s), asserted: lit)
            }
            let line = MicroBitV1Display.drive(d)
            gpio.assert(line)
            spinDelay(holdSpins)
            gpio.release(line)
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
        _ = Mmio.read32(MicroBitV1Bindings.DISPLAY_ROW1_PORT_BASE + Nrf51GpioLayout.IN_OFF)
    }
}
