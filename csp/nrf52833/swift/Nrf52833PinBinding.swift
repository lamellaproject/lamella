// HAND-WRITTEN Swift descriptor for one board-wired nRF52833 output pin. Every field arrives
// as a generated literal from a board's *Bindings enum -- the control-line quad a `gpio-out`
// device row emits.
//
// This part has TWO GPIO ports at separate base addresses, so a pin is genuinely a (port base,
// bit mask) pair rather than a single index. Keeping them separate here is what lets a board
// mix pins from both ports without the consumer doing arithmetic on a flattened pin number.

/// One board-wired output pin: which port it lives on, which bit it is, and which level
/// asserts it. `asserted`/`released` speak in the board's terms so a driver never has to know
/// whether a line sources or sinks current.
public struct Nrf52833PinBinding {
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
