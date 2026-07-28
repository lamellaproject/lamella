// HAND-WRITTEN Swift layer-1 GPIO driver for the nRF51, parameterized by Nrf51PinBinding
// descriptors. Every register offset comes from the GENERATED Nrf51GpioLayout; nothing here is a
// Swift-local hardware literal.
//
// Requires the Swift `Mmio` namespace (volatile 32-bit load/store).
//
// Every write uses the SET/CLR register aliases rather than a read-modify-write of the whole-port
// OUT or DIR register. On a scanned display that is not a micro-optimization: the scan changes
// column lines many times a second while row lines are also changing, and a read-modify-write can
// lose one of those changes.

/// Layer-1 GPIO for the nRF51: drives board-wired pins described by `Nrf51PinBinding`
/// descriptors. Stateless -- every call names the pin it acts on.
public struct Nrf51Gpio {
    public init() {}

    /// Makes the pin an output, left RELEASED (not asserted) so a display does not flash during
    /// bring-up. PIN_CNF needs no touching: it resets to input-with-buffer-disconnected, and
    /// DIRSET is what makes a pin an output.
    public func configureOutput(_ pin: Nrf51PinBinding) {
        release(pin)
        Mmio.write32(pin.portBase + Nrf51GpioLayout.DIRSET_OFF, pin.mask)
    }

    /// Drives the pin to its ASSERTED level -- high for a sourcing line, low for a sinking one.
    public func assert(_ pin: Nrf51PinBinding) {
        let reg = pin.activeLow ? Nrf51GpioLayout.OUTCLR_OFF : Nrf51GpioLayout.OUTSET_OFF
        Mmio.write32(pin.portBase + reg, pin.mask)
    }

    /// Drives the pin to its RELEASED level -- the opposite of `assert`.
    public func release(_ pin: Nrf51PinBinding) {
        let reg = pin.activeLow ? Nrf51GpioLayout.OUTSET_OFF : Nrf51GpioLayout.OUTCLR_OFF
        Mmio.write32(pin.portBase + reg, pin.mask)
    }

    /// Asserts or releases in one call.
    public func write(_ pin: Nrf51PinBinding, asserted: Bool) {
        if asserted { assert(pin) } else { release(pin) }
    }
}
