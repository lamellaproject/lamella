// HAND-WRITTEN Swift descriptor for one board-wired SAMD21 output pin. Every field arrives as
// a generated literal from a board's *Bindings enum -- the control-line quad a `gpio-out`
// device row emits.
//
// A pin is a (port base, bit mask) pair rather than a single index because this family has more
// than one PORT group, and a board is free to wire indicators on either.

/// One board-wired output pin: which PORT group it lives on, which bit it is, and which level
/// asserts it. `asserted`/`released` speak in the board's terms, so a driver never has to know
/// whether a line drives an indicator high or pulls it low.
public struct Samd21PinBinding {
    /// The PORT group base address this pin lives on.
    public let portBase: UInt32
    /// The pin's bit mask within its group (1 << pin index).
    public let mask: UInt32
    /// True when driving the pin LOW asserts it.
    public let activeLow: Bool

    public init(portBase: UInt32, mask: UInt32, activeLow: Bool) {
        self.portBase = portBase
        self.mask = mask
        self.activeLow = activeLow
    }
}
