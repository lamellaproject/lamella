// HAND-WRITTEN Swift descriptor for one board-wired nRF51 output pin. Every field arrives as a
// generated literal from a board's *Bindings enum -- the control-line quad a `gpio-out` device
// row emits.
//
// A pin still carries its port base rather than just an index, even though this part has only ONE
// GPIO port. That is deliberate: the descriptor shape then matches the nRF52833's, so a driver
// written against either reads the same, and a future two-port part in this family needs no
// change here.

/// One board-wired output pin: which port it lives on, which bit it is, and which level asserts
/// it. `asserted`/`released` speak in the board's terms, so a driver never has to know whether a
/// line sources or sinks current.
public struct Nrf51PinBinding {
    /// The GPIO port instance base this pin lives on.
    public let portBase: UInt32
    /// The pin's bit mask within its port (1 << pin index).
    public let mask: UInt32
    /// True when driving the pin LOW asserts it (a current-sinking line).
    public let activeLow: Bool

    public init(portBase: UInt32, mask: UInt32, activeLow: Bool) {
        self.portBase = portBase
        self.mask = mask
        self.activeLow = activeLow
    }
}
