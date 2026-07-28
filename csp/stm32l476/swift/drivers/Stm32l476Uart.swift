// HAND-WRITTEN Swift layer-1 driver for the STM32L476 USART, parameterized by a
// Stm32l476UsartBinding descriptor. Every register offset, field mask, and access width comes
// from the GENERATED Stm32l476UsartLayout; nothing here is a Swift-local hardware literal. The
// composed CR1 enable word is DRIVER-composed from those fields, not emitted.
//
// Requires the Swift `Mmio` namespace (volatile 8/16/32-bit load/store). Embedded-Swift subset
// only: caseless-enum namespaces + structs, no existentials, no reflection.

/// ONE STM32L476 USART driver for every USART instance on every board of the family,
/// parameterized by a Stm32l476UsartBinding. Absolute addresses are computed ONCE in `init`
/// into `let` fields, so hot loops touch fields and locals only.
///
/// Every register on this block is 32-bit word-accessible, so unlike the sub-word SERCOM
/// families there is no per-register width discipline to observe -- the generated `*_WIDTH`
/// constants all read 32.
///
/// Frame policy is 8N1, 16x oversampling, no parity, no flow control -- which on this block is
/// entirely the RESET state of CR1/CR2/CR3, so bring-up sets only the bits it needs (UE, TE, RE)
/// and deliberately leaves CR2 and CR3 untouched rather than writing back values it would have
/// to assume.
public struct Stm32l476Uart {
    private let cr1: UInt32
    private let brr: UInt32
    private let isr: UInt32
    private let rdr: UInt32
    private let tdr: UInt32
    private let binding: Stm32l476UsartBinding

    /// Binds the driver to one USART wiring; no hardware is touched until `initialize()`.
    public init(_ binding: Stm32l476UsartBinding) {
        self.binding = binding
        self.cr1 = binding.base + Stm32l476UsartLayout.CR1_OFF
        self.brr = binding.base + Stm32l476UsartLayout.BRR_OFF
        self.isr = binding.base + Stm32l476UsartLayout.ISR_OFF
        self.rdr = binding.base + Stm32l476UsartLayout.RDR_OFF
        self.tdr = binding.base + Stm32l476UsartLayout.TDR_OFF
    }

    /// Brings the bound USART up at the binding's rate, 8N1: gates the port and USART clocks,
    /// puts the bound pins in alternate-function mode and selects the function, sets the
    /// plan-derived divisor while disabled, then enables. Idempotent -- safe over a USART a
    /// previous image already configured, because every write is either a full-word assignment
    /// or a masked read-modify-write.
    ///
    /// Clock order matters: a peripheral's registers do not retain writes while its bus clock
    /// is gated, so both enables precede every other write here.
    public func initialize() {
        // Clocks: the GPIO port and the USART sit on DIFFERENT RCC banks on this family; both
        // registers and both bit masks are generated facts, so this reads the same for any
        // board of the family.
        Mmio.write32(binding.portRccEnReg,
                     Mmio.read32(binding.portRccEnReg) | binding.portRccEnMask)
        Mmio.write32(binding.rccEnReg,
                     Mmio.read32(binding.rccEnReg) | binding.rccEnMask)

        // Pins: alternate-function mode, then which alternate function. Masked writes so the
        // port's other pins keep their configuration.
        Mmio.write32(binding.moderReg,
                     (Mmio.read32(binding.moderReg) & ~binding.moderMask) | binding.moderValue)
        Mmio.write32(binding.afrlReg,
                     (Mmio.read32(binding.afrlReg) & ~binding.afrlMask) | binding.afrlValue)

        // Configure while disabled, then enable. The divisor is only latched out of reset with
        // UE clear, so CR1 is cleared first even if this image did not set it.
        Mmio.write32(cr1, 0)
        Mmio.write32(brr, binding.brrDivisor)
        Mmio.write32(cr1, Stm32l476UsartLayout.CR1_UE
                        | Stm32l476UsartLayout.CR1_TE
                        | Stm32l476UsartLayout.CR1_RE)
    }

    /// Sends one byte, waiting (bounded) for the transmit register to have room first.
    public func writeByte(_ value: UInt8) {
        for _ in 0 ..< 1_000_000 {
            if (Mmio.read32(isr) & Stm32l476UsartLayout.ISR_TXE) != 0 { break }
        }
        Mmio.write32(tdr, UInt32(value))
    }

    /// Sends a compile-time string as its low-byte (ASCII) characters -- the greeting path,
    /// allocation-free (StaticString, no managed string).
    public func write(_ text: StaticString) {
        text.withUTF8Buffer { buffer in
            for byte in buffer { writeByte(byte) }
        }
    }

    /// Sends a caller-owned byte buffer -- the neutral `uart.write(buf, len)` shape (the caller
    /// owns the buffer; the driver retains nothing past return; allocation-free).
    public func write(_ bytes: UnsafeBufferPointer<UInt8>) {
        for byte in bytes { writeByte(byte) }
    }

    /// Waits (bounded) until the last frame has left the shift register, so a mode switch or a
    /// caller's delay never cuts a frame on the wire.
    public func flush() {
        for _ in 0 ..< 1_000_000 {
            if (Mmio.read32(isr) & Stm32l476UsartLayout.ISR_TC) != 0 { return }
        }
    }

    /// 1 when at least one received byte waits, else 0.
    public var available: Int {
        (Mmio.read32(isr) & Stm32l476UsartLayout.ISR_RXNE) != 0 ? 1 : 0
    }

    /// Pops one received byte (the data field is 9 bits wide), or -1 when the receive register
    /// is empty -- the Stream convention: no waiting byte is data, never an error.
    public func readByte() -> Int {
        if (Mmio.read32(isr) & Stm32l476UsartLayout.ISR_RXNE) == 0 { return -1 }
        return Int(Mmio.read32(rdr) & Stm32l476UsartLayout.RDR_RDR)
    }
}
