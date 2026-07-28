// HAND-WRITTEN thin board class for the Adafruit Feather M0 Express in Swift: turns the
// GENERATED FeatherM0ExpressBindings constants into pin descriptors. Nothing here is a hardware
// literal -- every address, mask and polarity comes from the generated bindings, which come from
// bsp/feather-m0-express/board.toml.

/// The Adafruit Feather M0 Express, as its on-board indicators.
public enum FeatherM0Express {
    /// The red LED beside the USB jack.
    public static var led: Samd21PinBinding {
        Samd21PinBinding(portBase: FeatherM0ExpressBindings.LED_PORT_BASE,
                         mask: FeatherM0ExpressBindings.LED_MASK,
                         activeLow: FeatherM0ExpressBindings.LED_ACTIVE_LOW == 1)
    }

    /// The on-board addressable RGB LED's data line. One pin carrying a timed serial protocol,
    /// so a driver owns the waveform; the board only says which pin reaches it.
    public static var neopixel: Samd21PinBinding {
        Samd21PinBinding(portBase: FeatherM0ExpressBindings.NEOPIXEL_PORT_BASE,
                         mask: FeatherM0ExpressBindings.NEOPIXEL_MASK,
                         activeLow: FeatherM0ExpressBindings.NEOPIXEL_ACTIVE_LOW == 1)
    }
}
