// HAND-WRITTEN Swift descriptor for the STM32L476 USART driver -- the value bundle one bound
// USART wiring resolves to. Every field arrives as a generated literal from a board's
// *Bindings enum; the driver never holds a base address, a pin register, or a divisor of its
// own.
//
// The pin-mode and alternate-function words are per-PORT rather than per-pin: this family's
// USART binding routes its TX and RX pins through ONE MODER read-modify-write and ONE AFRL
// read-modify-write, because a bound pair sits in the same halves of both registers. That is
// why a single reg/mask/value triple describes each, and it is the generator's resolution --
// not an assumption this file is free to make.

/// The descriptor a STM32L476 USART driver consumes: one binding's resolved values, exactly
/// the constants a board's generated *Bindings enum carries. A board class constructs it from
/// those generated literals.
public struct Stm32l476UsartBinding {
    /// The bound USART instance's base address.
    public let base: UInt32
    /// The RCC register gating the USART instance's clock, and its enable bit as a mask.
    public let rccEnReg: UInt32
    public let rccEnMask: UInt32
    /// The RCC register gating the GPIO PORT the pins live on, and its enable bit as a mask.
    /// A separate bank from the USART's on this family -- that split is a generated fact.
    public let portRccEnReg: UInt32
    public let portRccEnMask: UInt32
    /// The port's pin-mode register, the mask covering the bound pins, and the alternate-function
    /// mode value for them.
    public let moderReg: UInt32
    public let moderMask: UInt32
    public let moderValue: UInt32
    /// The port's alternate-function-low register, the mask covering the bound pins, and the
    /// alternate-function selection for them.
    public let afrlReg: UInt32
    public let afrlMask: UInt32
    public let afrlValue: UInt32
    /// The BRR divisor for the binding's wire rate under the board's default clock plan
    /// (plan-derived; never authored).
    public let brrDivisor: UInt32

    public init(base: UInt32, rccEnReg: UInt32, rccEnMask: UInt32,
                portRccEnReg: UInt32, portRccEnMask: UInt32,
                moderReg: UInt32, moderMask: UInt32, moderValue: UInt32,
                afrlReg: UInt32, afrlMask: UInt32, afrlValue: UInt32,
                brrDivisor: UInt32) {
        self.base = base
        self.rccEnReg = rccEnReg
        self.rccEnMask = rccEnMask
        self.portRccEnReg = portRccEnReg
        self.portRccEnMask = portRccEnMask
        self.moderReg = moderReg
        self.moderMask = moderMask
        self.moderValue = moderValue
        self.afrlReg = afrlReg
        self.afrlMask = afrlMask
        self.afrlValue = afrlValue
        self.brrDivisor = brrDivisor
    }
}
