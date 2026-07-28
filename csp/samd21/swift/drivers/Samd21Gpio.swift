// HAND-WRITTEN Swift layer-1 GPIO driver for the SAMD21, parameterized by Samd21PinBinding
// descriptors. Every register offset comes from the GENERATED Samd21PortLayout; nothing here is
// a Swift-local hardware literal.
//
// Requires the Swift `Mmio` namespace (volatile 32-bit load/store).
//
// Every write uses the port's SET/CLR register aliases rather than a read-modify-write of the
// whole-group OUT or DIR register: a read-modify-write can lose a concurrent change to another
// board line sharing the group, and boards do share groups.

/// Layer-1 GPIO for the SAMD21: drives board-wired pins described by `Samd21PinBinding`
/// descriptors. Stateless -- every call names the pin it acts on, so one instance serves every
/// pin in every PORT group.
public struct Samd21Gpio {
    public init() {}

    /// Makes the pin an output, left RELEASED (not asserted) so an indicator does not flash
    /// during bring-up.
    public func configureOutput(_ pin: Samd21PinBinding) {
        release(pin)
        Mmio.write32(pin.portBase + Samd21PortLayout.DIRSET_OFF, pin.mask)
    }

    /// Drives the pin to its ASSERTED level.
    public func assert(_ pin: Samd21PinBinding) {
        let reg = pin.activeLow ? Samd21PortLayout.OUTCLR_OFF : Samd21PortLayout.OUTSET_OFF
        Mmio.write32(pin.portBase + reg, pin.mask)
    }

    /// Drives the pin to its RELEASED level -- the opposite of `assert`.
    public func release(_ pin: Samd21PinBinding) {
        let reg = pin.activeLow ? Samd21PortLayout.OUTSET_OFF : Samd21PortLayout.OUTCLR_OFF
        Mmio.write32(pin.portBase + reg, pin.mask)
    }

    /// Asserts or releases in one call.
    public func write(_ pin: Samd21PinBinding, asserted: Bool) {
        if asserted { assert(pin) } else { release(pin) }
    }
}
