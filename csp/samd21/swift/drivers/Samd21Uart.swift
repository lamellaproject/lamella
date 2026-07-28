// HAND-WRITTEN Swift layer-1 driver for the SAMD21 SERCOM-USART -- the Swift projection of
// csp/samd21/csharp/drivers/Samd21Uart.cs, parameterized by a Samd21SercomUsartBinding
// descriptor. Every register offset, field, and width comes from the GENERATED Swift block
// layouts; nothing here is a Swift-local hardware literal. The composed CTRLA words
// (0x40310084 pad2/pad3, 0x40100084 pad0/pad1) are DRIVER-composed from those fields --
// byte-identical to the C# driver's composition -- not emitted.
//
// Requires the Swift `Mmio` namespace (volatile 8/16/32-bit load/store -- the Swift analog
// of C#'s Lamella.Hardware.Mmio, lowered by the AOT tier). Embedded-Swift subset only:
// caseless-enum namespaces + structs, no existentials, no reflection.

/// ONE SAMD21 SERCOM-USART driver for every SERCOM on every SAMD21 board, parameterized by
/// a Samd21SercomUsartBinding. Absolute addresses are computed ONCE in `init` into `let`
/// fields (hot loops touch fields and locals). The SAMD21 needs SUB-WORD MMIO (a Cortex-M0+
/// faults on an unaligned 32-bit access): CLKCTRL is 16-bit at an unaligned offset, STATUS
/// and the PORT mux/config bytes are 8-bit -- reached with write16/read8 per each
/// register's generated *_WIDTH.
///
/// Frame policy (8N1, LSB-first, run-in-standby, 16x oversampling) is DRIVER policy
/// composed from layout fields at init; the composition is pinned by the no-drift gate.
public struct Samd21Uart {
    private let ctrla: UInt32
    private let ctrlb: UInt32
    private let baud: UInt32
    private let intflag: UInt32
    private let syncbusy: UInt32
    private let data: UInt32
    private let binding: Samd21SercomUsartBinding

    /// Binds the driver to one SERCOM-USART wiring; no hardware is touched until `initialize()`.
    public init(_ binding: Samd21SercomUsartBinding) {
        self.binding = binding
        self.ctrla = binding.sercomBase + Samd21SercomUsartLayout.CTRLA_OFF
        self.ctrlb = binding.sercomBase + Samd21SercomUsartLayout.CTRLB_OFF
        self.baud = binding.sercomBase + Samd21SercomUsartLayout.BAUD_OFF
        self.intflag = binding.sercomBase + Samd21SercomUsartLayout.INTFLAG_OFF
        self.syncbusy = binding.sercomBase + Samd21SercomUsartLayout.SYNCBUSY_OFF
        self.data = binding.sercomBase + Samd21SercomUsartLayout.DATA_OFF
    }

    /// Brings the bound SERCOM up as a USART at the binding's rate, 8N1 LSB-first: gates the
    /// APB clock, routes the core clock per the binding's composed GCLK word, muxes TX/RX
    /// (RX input buffer ON), configures, then enables -- each enable-protected write waiting
    /// its SYNCBUSY bit. Idempotent -- safe over a SERCOM the resident firmware already configured.
    public func initialize() {
        // Clock: gate the instance's APB clock, route its core clock, wait GCLK sync.
        let apbcmask = Samd21Instances.PM_BASE + Samd21PmLayout.APBCMASK_OFF
        Mmio.write32(apbcmask, Mmio.read32(apbcmask) | binding.apbcMask)
        Mmio.write16(Samd21Instances.GCLK_BASE + Samd21GclkLayout.CLKCTRL_OFF,
                     UInt16(truncatingIfNeeded: binding.gclkClkctrlValue))
        while (Mmio.read8(Samd21Instances.GCLK_BASE + Samd21GclkLayout.STATUS_OFF)
               & UInt8(truncatingIfNeeded: Samd21GclkLayout.STATUS_SYNCBUSY)) != 0 {}

        // Pinmux: both pins to the binding's function; RX additionally needs INEN (without
        // it the SERCOM never sees the incoming line -- the classic SAMD21 UART gotcha).
        Mmio.write8(binding.pmuxReg, UInt8(truncatingIfNeeded: binding.pmuxPair))
        Mmio.write8(binding.pincfgTxReg, UInt8(truncatingIfNeeded: Samd21PortLayout.PINCFG0_PMUXEN))
        Mmio.write8(binding.pincfgRxReg,
                    UInt8(truncatingIfNeeded: Samd21PortLayout.PINCFG0_PMUXEN
                                              | Samd21PortLayout.PINCFG0_INEN))

        // CTRLA (while disabled): USART with internal clock, the binding's pad routing,
        // LSB-first, run-in-standby; then the plan-derived divisor; then TXEN|RXEN; each
        // write synchronized.
        let ctrlaWord =
            (Samd21SercomUsartLayout.CTRLA_MODE_USART_INTCLK << Samd21SercomUsartLayout.CTRLA_MODE_LSB)
            | Samd21SercomUsartLayout.CTRLA_RUNSTDBY
            | (binding.txpo << Samd21SercomUsartLayout.CTRLA_TXPO_LSB)
            | (binding.rxpo << Samd21SercomUsartLayout.CTRLA_RXPO_LSB)
            | Samd21SercomUsartLayout.CTRLA_DORD
        Mmio.write32(ctrla, ctrlaWord)
        Mmio.write16(baud, UInt16(truncatingIfNeeded: binding.baudDivisor))
        Mmio.write32(ctrlb, Samd21SercomUsartLayout.CTRLB_TXEN | Samd21SercomUsartLayout.CTRLB_RXEN)
        while (Mmio.read32(syncbusy) & Samd21SercomUsartLayout.SYNCBUSY_CTRLB) != 0 {}
        Mmio.write32(ctrla, Mmio.read32(ctrla) | Samd21SercomUsartLayout.CTRLA_ENABLE)
        while (Mmio.read32(syncbusy) & Samd21SercomUsartLayout.SYNCBUSY_ENABLE) != 0 {}
    }

    /// Sends one byte, waiting (bounded) for the Data Register Empty flag first.
    public func writeByte(_ value: UInt8) {
        for _ in 0 ..< 1_000_000 {
            if (Mmio.read8(intflag) & UInt8(truncatingIfNeeded: Samd21SercomUsartLayout.INTFLAG_DRE)) != 0 { break }
        }
        Mmio.write16(data, UInt16(value))
    }

    /// Sends a compile-time string as its low-byte (ASCII) characters -- the greeting path,
    /// allocation-free (StaticString, no managed string).
    public func write(_ text: StaticString) {
        text.withUTF8Buffer { buffer in
            for byte in buffer { writeByte(byte) }
        }
    }

    /// Sends a caller-owned byte buffer -- the neutral `uart.write(buf, len)` shape (the
    /// caller owns the buffer; the driver retains nothing past return; allocation-free).
    public func write(_ bytes: UnsafeBufferPointer<UInt8>) {
        for byte in bytes { writeByte(byte) }
    }

    /// Waits (bounded) until the last frame has left the shift register (INTFLAG.TXC), so a
    /// mode switch or a caller's delay never cuts a frame on the wire.
    public func flush() {
        for _ in 0 ..< 1_000_000 {
            if (Mmio.read8(intflag) & UInt8(truncatingIfNeeded: Samd21SercomUsartLayout.INTFLAG_TXC)) != 0 { return }
        }
    }

    /// 1 when at least one received byte waits (INTFLAG.RXC), else 0.
    public var available: Int {
        (Mmio.read8(intflag) & UInt8(truncatingIfNeeded: Samd21SercomUsartLayout.INTFLAG_RXC)) != 0 ? 1 : 0
    }

    /// Pops one received byte (the DATA field is 9 bits wide), or -1 when the RX register is
    /// empty (the Stream convention: no waiting byte is data, never an error).
    public func readByte() -> Int {
        if (Mmio.read8(intflag) & UInt8(truncatingIfNeeded: Samd21SercomUsartLayout.INTFLAG_RXC)) == 0 { return -1 }
        let raw = UInt32(Mmio.read16(data))
        return Int(raw & Samd21SercomUsartLayout.DATA_DATA)
    }
}
