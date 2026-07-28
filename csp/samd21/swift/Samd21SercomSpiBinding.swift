// HAND-WRITTEN Swift descriptor for the SAMD21 SERCOM-SPI driver: one binding's resolved
// values, exactly the constants a board's generated *Bindings enum carries. The driver never
// holds an instance base, a pin register, a mux value or a divisor of its own.
//
// THIS SHAPE FOLLOWS THE EMISSION, not the other way round. An earlier version of this file
// mirrored the USART descriptor and took a COMPOSED pmux pair byte -- but the samd21 spi
// emission arm states each signal separately, as (pmux register, nibble shift, pincfg
// register). That is the better shape and the descriptor was wrong to assume otherwise: a
// SERCOM's three SPI signals need not share a PMUX byte, and on this board they do not (MOSI
// and SCK are PB10/PB11, an even/odd pair sharing one byte at shifts 0 and 4, while MISO is
// PA12 in a different PORT group entirely).

/// The descriptor a SAMD21 SERCOM-SPI driver consumes.
///
/// The pin fields are where SPI differs from the USART pair. A SERCOM SPI master drives three
/// signals out (DO, SCK, and optionally SS) and samples one in (DI), and the pad routing is not
/// per-pin: DOPO selects a whole TRIO of pads for the outputs, DIPO selects the one input pad.
public struct Samd21SercomSpiBinding {
    /// The bound SERCOM instance's base address.
    public let sercomBase: UInt32
    /// The instance's GCLK core-clock id, UNSHIFTED -- the driver composes the CLKCTRL word
    /// (id | generator | CLKEN) because which generator this runs from is a PLAN choice, not a
    /// board fact. (The USART binding carries a pre-composed word; this one deliberately does
    /// not, and the emitted comment says so.)
    public let gclkCoreId: UInt32
    /// The instance's PM.APBCMASK gate bit, as a mask.
    public let apbcMask: UInt32
    /// The PMUX nibble VALUE the binding's function letter resolves to, generated -- so no
    /// driver carries a mux literal of its own.
    public let pmuxFunc: UInt32
    /// The resolved PORT PMUX byte address of the DO (MOSI) pin, and the nibble shift within it.
    public let pmuxMosiReg: UInt32
    public let pmuxMosiShift: UInt32
    /// The resolved PORT PINCFG byte address of the DO (MOSI) pin.
    public let pincfgMosiReg: UInt32
    /// The SCK pin's PMUX byte address, nibble shift, and PINCFG address.
    public let pmuxSckReg: UInt32
    public let pmuxSckShift: UInt32
    public let pincfgSckReg: UInt32
    /// The DI (MISO) pin's PMUX byte address, nibble shift, and PINCFG address.
    public let pmuxMisoReg: UInt32
    public let pmuxMisoShift: UInt32
    public let pincfgMisoReg: UInt32
    /// The CTRLA.DOPO value selecting the output pad trio (DO/SCK/SS).
    public let dopo: UInt32
    /// The CTRLA.DIPO value selecting the input pad.
    public let dipo: UInt32
    /// The BAUD divisor for the wanted clock under the driver's plan (synchronous:
    /// BAUD = f_ref / (2 * f_sck) - 1).
    public let baudDivisor: UInt32
    /// The SPI mode's clock polarity (CTRLA.CPOL): 0 = idle low, 1 = idle high.
    public let cpol: UInt32
    /// The SPI mode's clock phase (CTRLA.CPHA): 0 = sample leading edge, 1 = trailing.
    public let cpha: UInt32

    public init(sercomBase: UInt32, gclkCoreId: UInt32, apbcMask: UInt32, pmuxFunc: UInt32,
                pmuxMosiReg: UInt32, pmuxMosiShift: UInt32, pincfgMosiReg: UInt32,
                pmuxSckReg: UInt32, pmuxSckShift: UInt32, pincfgSckReg: UInt32,
                pmuxMisoReg: UInt32, pmuxMisoShift: UInt32, pincfgMisoReg: UInt32,
                dopo: UInt32, dipo: UInt32, baudDivisor: UInt32,
                cpol: UInt32, cpha: UInt32) {
        self.sercomBase = sercomBase
        self.gclkCoreId = gclkCoreId
        self.apbcMask = apbcMask
        self.pmuxFunc = pmuxFunc
        self.pmuxMosiReg = pmuxMosiReg
        self.pmuxMosiShift = pmuxMosiShift
        self.pincfgMosiReg = pincfgMosiReg
        self.pmuxSckReg = pmuxSckReg
        self.pmuxSckShift = pmuxSckShift
        self.pincfgSckReg = pincfgSckReg
        self.pmuxMisoReg = pmuxMisoReg
        self.pmuxMisoShift = pmuxMisoShift
        self.pincfgMisoReg = pincfgMisoReg
        self.dopo = dopo
        self.dipo = dipo
        self.baudDivisor = baudDivisor
        self.cpol = cpol
        self.cpha = cpha
    }
}
