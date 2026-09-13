// HAND-WRITTEN thin board class for the Adafruit Feather M0 WiFi in Swift: turns the GENERATED
// AdafruitFeatherM0WifiBindings constants into pin descriptors. Nothing here is a hardware literal --
// every address, mask and polarity comes from the generated bindings, which come from
// bsp/adafruit-feather-m0-wifi/board.toml.

/// The Adafruit Feather M0 WiFi, as its indicator and its WINC1500's control lines.
///
/// The three WINC lines are ordinary GPIO on this board -- the SPI carries the data, but reset,
/// chip-enable and the interrupt are levels the host drives or reads. Their POLARITY is board
/// truth and is carried here rather than assumed: RESET_N and IRQN are active low, CHIP_EN is
/// active high, so `assert(wincReset)` holds the part in reset whichever way the line is wired.
public enum FeatherM0Wifi {
    /// The red LED beside the USB jack.
    public static var led: Samd21PinBinding {
        Samd21PinBinding(portBase: AdafruitFeatherM0WifiBindings.LED_PORT_BASE,
                         mask: AdafruitFeatherM0WifiBindings.LED_MASK,
                         activeLow: AdafruitFeatherM0WifiBindings.LED_ACTIVE_LOW == 1)
    }

    /// The WINC1500's reset line -- asserted (low) holds the part in reset.
    public static var wincReset: Samd21PinBinding {
        Samd21PinBinding(portBase: AdafruitFeatherM0WifiBindings.WINC_RESET_N_PORT_BASE,
                         mask: AdafruitFeatherM0WifiBindings.WINC_RESET_N_MASK,
                         activeLow: AdafruitFeatherM0WifiBindings.WINC_RESET_N_ACTIVE_LOW == 1)
    }

    /// The WINC1500's chip enable -- asserted (high) powers the part up.
    public static var wincChipEnable: Samd21PinBinding {
        Samd21PinBinding(portBase: AdafruitFeatherM0WifiBindings.WINC_CHIP_EN_PORT_BASE,
                         mask: AdafruitFeatherM0WifiBindings.WINC_CHIP_EN_MASK,
                         activeLow: AdafruitFeatherM0WifiBindings.WINC_CHIP_EN_ACTIVE_LOW == 1)
    }

    /// The WINC1500's interrupt line -- an INPUT: the module asserts it (low) when a message
    /// waits. Carried as the same descriptor type; only the direction a driver configures differs.
    public static var wincInterrupt: Samd21PinBinding {
        Samd21PinBinding(portBase: AdafruitFeatherM0WifiBindings.WINC_IRQN_PORT_BASE,
                         mask: AdafruitFeatherM0WifiBindings.WINC_IRQN_MASK,
                         activeLow: AdafruitFeatherM0WifiBindings.WINC_IRQN_ACTIVE_LOW == 1)
    }

    /// The WINC1500's chip select -- a SOFT select: the SERCOM does not drive it, the driver
    /// does, which is what lets one bus serve more than one device. Active low is the SPI
    /// convention rather than a board fact, so unlike the lines above it is stated here and not
    /// read from a generated `_ACTIVE_LOW` (the spi binding emits none).
    public static var wincChipSelect: Samd21PinBinding {
        Samd21PinBinding(portBase: AdafruitFeatherM0WifiBindings.WINC_SPI_CS_PORT_BASE,
                         mask: AdafruitFeatherM0WifiBindings.WINC_SPI_CS_MASK,
                         activeLow: true)
    }

    /// The SERCOM4 SPI wiring that reaches the WINC1500.
    ///
    /// `baudDivisor` and the clock mode are the CALLER's operating point, not board truth: the
    /// synchronous divisor is f_ref / (2 * f_sck) - 1, and f_ref is whatever generator the
    /// driver routes. SPI mode 0 (CPOL 0, CPHA 0) is what the WINC1500 expects.
    public static func wincSpi(baudDivisor: UInt32) -> Samd21SercomSpiBinding {
        Samd21SercomSpiBinding(
            sercomBase: AdafruitFeatherM0WifiBindings.WINC_SPI_SERCOM_BASE,
            gclkCoreId: AdafruitFeatherM0WifiBindings.WINC_SPI_GCLK_CORE_ID,
            apbcMask: AdafruitFeatherM0WifiBindings.WINC_SPI_APBC_MASK,
            pmuxFunc: AdafruitFeatherM0WifiBindings.WINC_SPI_PMUX_FUNC,
            pmuxMosiReg: AdafruitFeatherM0WifiBindings.WINC_SPI_PMUX_MOSI_REG,
            pmuxMosiShift: AdafruitFeatherM0WifiBindings.WINC_SPI_PMUX_MOSI_SHIFT,
            pincfgMosiReg: AdafruitFeatherM0WifiBindings.WINC_SPI_PINCFG_MOSI_REG,
            pmuxSckReg: AdafruitFeatherM0WifiBindings.WINC_SPI_PMUX_SCK_REG,
            pmuxSckShift: AdafruitFeatherM0WifiBindings.WINC_SPI_PMUX_SCK_SHIFT,
            pincfgSckReg: AdafruitFeatherM0WifiBindings.WINC_SPI_PINCFG_SCK_REG,
            pmuxMisoReg: AdafruitFeatherM0WifiBindings.WINC_SPI_PMUX_MISO_REG,
            pmuxMisoShift: AdafruitFeatherM0WifiBindings.WINC_SPI_PMUX_MISO_SHIFT,
            pincfgMisoReg: AdafruitFeatherM0WifiBindings.WINC_SPI_PINCFG_MISO_REG,
            dopo: AdafruitFeatherM0WifiBindings.WINC_SPI_DOPO,
            dipo: AdafruitFeatherM0WifiBindings.WINC_SPI_DIPO,
            baudDivisor: baudDivisor,
            cpol: 0,
            cpha: 0)
    }
}
