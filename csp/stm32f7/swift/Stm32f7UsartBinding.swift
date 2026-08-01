// HAND-WRITTEN Swift descriptor for the STM32F7 USART driver -- the value bundle one bound USART
// wiring resolves to. Every field arrives as a generated literal from a board's *Bindings enum;
// the driver never holds a base address, a pin register, or a divisor of its own.
//
// THE TX AND RX MUX FACTS ARE SEPARATE, one descriptor each, and on this family that is not a
// formality: a board may transmit on port A and receive on port B, and a transmit pin above 7 puts
// its alternate-function nibble in a different register than the receive pin's.
// Two pins of one USART may share nothing at all.

/// The resolved mux facts for ONE bound pin: the port clock gate to open, and the two
/// read-modify-writes that put the pin into alternate-function mode on its function number.
public struct Stm32f7UsartPinMux {
    /// The RCC register gating this pin's port clock, and that port's enable bit as a mask.
    public let portRccEnReg: UInt32
    public let portRccEnMask: UInt32
    /// The port's pin-mode register, this pin's field span, and the alternate-function mode value.
    public let moderReg: UInt32
    public let moderMask: UInt32
    public let moderValue: UInt32
    /// The alternate-function register covering this pin -- AFRL for pins 0..7, AFRH for pins
    /// 8..15 -- this pin's nibble, and the function selection. Which register it is was decided
    /// when the binding resolved, so there is no pin arithmetic left here.
    public let afrReg: UInt32
    public let afrMask: UInt32
    public let afrValue: UInt32

    public init(portRccEnReg: UInt32, portRccEnMask: UInt32,
                moderReg: UInt32, moderMask: UInt32, moderValue: UInt32,
                afrReg: UInt32, afrMask: UInt32, afrValue: UInt32) {
        self.portRccEnReg = portRccEnReg
        self.portRccEnMask = portRccEnMask
        self.moderReg = moderReg
        self.moderMask = moderMask
        self.moderValue = moderValue
        self.afrReg = afrReg
        self.afrMask = afrMask
        self.afrValue = afrValue
    }
}

/// The descriptor a STM32F7 USART driver consumes: one binding's resolved values, exactly the
/// constants a board's generated *Bindings enum carries. A board class constructs it from those
/// generated literals.
public struct Stm32f7UsartBinding {
    /// The bound USART instance's base address.
    public let base: UInt32
    /// The RCC register gating the USART instance's clock, and its enable bit as a mask.
    public let rccEnReg: UInt32
    public let rccEnMask: UInt32
    /// The transmit and receive pins' mux facts.
    public let tx: Stm32f7UsartPinMux
    public let rx: Stm32f7UsartPinMux
    /// The BRR divisor for the binding's wire rate under the board's default clock plan
    /// (plan-derived; never authored).
    public let brrDivisor: UInt32

    public init(base: UInt32, rccEnReg: UInt32, rccEnMask: UInt32,
                tx: Stm32f7UsartPinMux, rx: Stm32f7UsartPinMux,
                brrDivisor: UInt32) {
        self.base = base
        self.rccEnReg = rccEnReg
        self.rccEnMask = rccEnMask
        self.tx = tx
        self.rx = rx
        self.brrDivisor = brrDivisor
    }
}
