// HAND-WRITTEN Swift layer-1 GPIO driver for the nRF52833, parameterized by
// Nrf52833PinBinding descriptors. Every register offset comes from the GENERATED
// Nrf52833GpioLayout; nothing here is a Swift-local hardware literal.
//
// Requires the Swift `Mmio` namespace (volatile 32-bit load/store).
//
// Every operation uses the SET/CLR register aliases rather than a read-modify-write of OUT or
// DIR. That is not a micro-optimization: a read-modify-write of a whole-port register can lose
// a concurrent change to another pin on the same port, and this part puts unrelated board lines
// on the same ports.

/// Layer-1 GPIO for the nRF52833: drives board-wired pins described by
/// `Nrf52833PinBinding` descriptors. Stateless -- every call names the pin it acts on, so one
/// instance serves every pin on both ports.
public struct Nrf52833Gpio {
    public init() {}

    /// Makes the pin an output, left RELEASED (not asserted) so a display or an LED does not
    /// flash during bring-up.
    public func configureOutput(_ pin: Nrf52833PinBinding) {
        release(pin)
        Mmio.write32(pin.portBase + Nrf52833GpioLayout.DIRSET_OFF, pin.mask)
    }

    /// Drives the pin to its ASSERTED level -- high for a sourcing line, low for a sinking one.
    public func assert(_ pin: Nrf52833PinBinding) {
        let reg = pin.activeLow ? Nrf52833GpioLayout.OUTCLR_OFF : Nrf52833GpioLayout.OUTSET_OFF
        Mmio.write32(pin.portBase + reg, pin.mask)
    }

    /// Drives the pin to its RELEASED level -- the opposite of `assert`.
    public func release(_ pin: Nrf52833PinBinding) {
        let reg = pin.activeLow ? Nrf52833GpioLayout.OUTSET_OFF : Nrf52833GpioLayout.OUTCLR_OFF
        Mmio.write32(pin.portBase + reg, pin.mask)
    }

    /// Asserts or releases in one call.
    public func write(_ pin: Nrf52833PinBinding, asserted: Bool) {
        if asserted { assert(pin) } else { release(pin) }
    }

    /// Drives several pins of ONE port at once -- the scanned-display case, where a whole row
    /// of columns changes together and doing it pin-by-pin would show as uneven brightness.
    /// `assertedMask` must be a subset of `portMask`; both are raw port bit masks.
    public func writePortGroup(portBase: UInt32, portMask: UInt32, assertedMask: UInt32,
                               activeLow: Bool) {
        let onReg = activeLow ? Nrf52833GpioLayout.OUTCLR_OFF : Nrf52833GpioLayout.OUTSET_OFF
        let offReg = activeLow ? Nrf52833GpioLayout.OUTSET_OFF : Nrf52833GpioLayout.OUTCLR_OFF
        if assertedMask != 0 { Mmio.write32(portBase + onReg, assertedMask) }
        let releasedMask = portMask & ~assertedMask
        if releasedMask != 0 { Mmio.write32(portBase + offReg, releasedMask) }
    }
}
