// HAND-WRITTEN Swift layer-1 GPIO driver for the SAM E5x, parameterized by Same54PinBinding
// descriptors. Every register offset comes from the GENERATED Same54PortLayout; nothing here is
// a Swift-local hardware literal.
//
// Requires the Swift `Mmio` namespace (volatile 32-bit load/store).
//
// Every write uses the port's SET/CLR register aliases rather than a read-modify-write of the
// whole-group OUT or DIR register: a read-modify-write can lose a concurrent change to another
// board line sharing the group, and boards do share groups.
//
// WHY THIS IS A SEPARATE FILE FROM THE SAMD21 ONE, given the two are line-for-line the same.
// The E5x PORT block is a strict SUPERSET of the D21's and the three registers this driver touches
// sit at identical offsets -- measured from the two `blocks/port.toml` files, not assumed:
//
//     DIRSET 0x08   OUTCLR 0x14   OUTSET 0x18      identical in both families
//     E5x also adds DIRTGL 0x0C, OUTTGL 0x1C, CTRL 0x24, WRCONFIG 0x28, EVCTRL 0x2C
//
// So the two drivers could share an implementation. They do not, because the TYPES do not: a
// Same54PinBinding and a Samd21PinBinding are different types over different generated layouts,
// and collapsing them would mean one family's driver reading the other family's offsets whenever
// the supersetting stopped being true. The duplication is two dozen lines; the alternative is a
// silent cross-family read the day a register moves.

/// Layer-1 GPIO for the SAM E5x: drives board-wired pins described by `Same54PinBinding`
/// descriptors. Stateless -- every call names the pin it acts on, so one instance serves every
/// pin in every PORT group.
public struct Same54Gpio {
    public init() {}

    /// Makes the pin an output, left RELEASED (not asserted) so an indicator does not flash
    /// during bring-up.
    public func configureOutput(_ pin: Same54PinBinding) {
        release(pin)
        Mmio.write32(pin.portBase + Same54PortLayout.DIRSET_OFF, pin.mask)
    }

    /// Drives the pin to its ASSERTED level.
    public func assert(_ pin: Same54PinBinding) {
        let reg = pin.activeLow ? Same54PortLayout.OUTCLR_OFF : Same54PortLayout.OUTSET_OFF
        Mmio.write32(pin.portBase + reg, pin.mask)
    }

    /// Drives the pin to its RELEASED level -- the opposite of `assert`.
    public func release(_ pin: Same54PinBinding) {
        let reg = pin.activeLow ? Same54PortLayout.OUTSET_OFF : Same54PortLayout.OUTCLR_OFF
        Mmio.write32(pin.portBase + reg, pin.mask)
    }

    /// Asserts or releases in one call.
    public func write(_ pin: Same54PinBinding, asserted: Bool) {
        if asserted { assert(pin) } else { release(pin) }
    }
}
