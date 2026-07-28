// HAND-WRITTEN Swift descriptor for the SAMD21 SERCOM-USART driver -- the Swift projection
// of csp/samd21/csharp/Samd21SercomUsartBinding.cs. Every value arrives as a generated
// literal from a board's *Bindings enum; the driver never holds an instance base, a pin
// register, or a divisor of its own.

/// The descriptor a SAMD21 SERCOM-USART driver consumes: one binding's resolved values,
/// exactly the constants a board's generated *Bindings enum carries. A board class
/// constructs it from generated literals.
public struct Samd21SercomUsartBinding {
    /// The bound SERCOM instance's base address.
    public let sercomBase: UInt32
    /// The composed GCLK.CLKCTRL word routing the instance's core clock (ID | generator | CLKEN).
    public let gclkClkctrlValue: UInt32
    /// The instance's PM.APBCMASK gate bit, as a mask.
    public let apbcMask: UInt32
    /// The resolved PORT PMUX byte address covering the TX/RX pin pair.
    public let pmuxReg: UInt32
    /// The composed PMUX byte (the mux function in both nibbles).
    public let pmuxPair: UInt32
    /// The resolved PORT PINCFG byte address of the TX pin.
    public let pincfgTxReg: UInt32
    /// The resolved PORT PINCFG byte address of the RX pin.
    public let pincfgRxReg: UInt32
    /// The CTRLA.TXPO pad-routing value for the TX pin.
    public let txpo: UInt32
    /// The CTRLA.RXPO pad-routing value for the RX pin.
    public let rxpo: UInt32
    /// The BAUD divisor for the binding's wire rate under the board's default clock plan
    /// (plan-derived; never authored).
    public let baudDivisor: UInt32

    public init(sercomBase: UInt32, gclkClkctrlValue: UInt32, apbcMask: UInt32,
                pmuxReg: UInt32, pmuxPair: UInt32, pincfgTxReg: UInt32, pincfgRxReg: UInt32,
                txpo: UInt32, rxpo: UInt32, baudDivisor: UInt32) {
        self.sercomBase = sercomBase
        self.gclkClkctrlValue = gclkClkctrlValue
        self.apbcMask = apbcMask
        self.pmuxReg = pmuxReg
        self.pmuxPair = pmuxPair
        self.pincfgTxReg = pincfgTxReg
        self.pincfgRxReg = pincfgRxReg
        self.txpo = txpo
        self.rxpo = rxpo
        self.baudDivisor = baudDivisor
    }
}
