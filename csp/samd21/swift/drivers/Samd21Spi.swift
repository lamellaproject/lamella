// HAND-WRITTEN Swift layer-1 driver for the SAMD21 SERCOM-SPI, parameterized by a
// Samd21SercomSpiBinding descriptor. Every register offset, field, and width comes from the
// GENERATED Samd21SercomSpiLayout; nothing here is a Swift-local hardware literal. The
// composed CTRLA word is DRIVER-composed from those fields, not emitted -- the same division
// of labor as Samd21Uart.
//
// Requires the Swift `Mmio` namespace (volatile 8/16/32-bit load/store). Embedded-Swift subset
// only: caseless-enum namespaces + structs, no existentials, no reflection.
//
// MASTER ONLY. The block file describes slave mode too (CTRLA.MODE 0x2), but a slave needs an
// address/select path this driver does not implement, and shipping a half-slave would be worse
// than shipping none.

/// ONE SAMD21 SERCOM-SPI master driver for every SERCOM on every SAMD21 board, parameterized
/// by a Samd21SercomSpiBinding. Absolute addresses are computed ONCE in `init` into `let`
/// fields (hot loops touch fields and locals). The SAMD21 needs SUB-WORD MMIO (a Cortex-M0+
/// faults on an unaligned 32-bit access): BAUD and the PORT mux/config bytes are 8-bit, DATA
/// is 16-bit -- reached with write8/read8/write16/read16 per each register's generated
/// *_WIDTH.
///
/// Frame policy (8-bit characters, MSB-first, software-controlled slave select) is DRIVER
/// policy composed from layout fields at init; the clock mode comes from the binding, because
/// which mode a device wants is a property of the DEVICE, not of the SERCOM.
///
/// The chip select is NOT this driver's business. MSSEN stays 0 (software-controlled SS) and
/// the board's CS line is an ordinary GPIO driven through Samd21Gpio -- which is what lets one
/// bus serve several devices, and what keeps a transfer's framing in the caller's hands.
public struct Samd21Spi {
    private let ctrla: UInt32
    private let ctrlb: UInt32
    private let baud: UInt32
    private let intflag: UInt32
    private let status: UInt32
    private let syncbusy: UInt32
    private let data: UInt32
    private let binding: Samd21SercomSpiBinding

    /// Binds the driver to one SERCOM-SPI wiring; no hardware is touched until `initialize()`.
    public init(_ binding: Samd21SercomSpiBinding) {
        self.binding = binding
        self.ctrla = binding.sercomBase + Samd21SercomSpiLayout.CTRLA_OFF
        self.ctrlb = binding.sercomBase + Samd21SercomSpiLayout.CTRLB_OFF
        self.baud = binding.sercomBase + Samd21SercomSpiLayout.BAUD_OFF
        self.intflag = binding.sercomBase + Samd21SercomSpiLayout.INTFLAG_OFF
        self.status = binding.sercomBase + Samd21SercomSpiLayout.STATUS_OFF
        self.syncbusy = binding.sercomBase + Samd21SercomSpiLayout.SYNCBUSY_OFF
        self.data = binding.sercomBase + Samd21SercomSpiLayout.DATA_OFF
    }

    /// Brings the bound SERCOM up as an SPI master at the binding's clock and mode, 8-bit
    /// MSB-first: gates the APB clock, routes the core clock, muxes DO/SCK/DI (DI needs the
    /// input buffer ON), configures, then enables -- each enable-protected write waiting its
    /// SYNCBUSY bit. Idempotent -- safe over a SERCOM the resident firmware already configured.
    public func initialize(gclkGenerator: UInt32 = 0) {
        // Clock: gate the instance's APB clock, route its core clock, wait GCLK sync. The
        // CLKCTRL word is composed HERE from the generated core id because the generator is a
        // plan choice: id | (generator << GEN_LSB) | CLKEN.
        let apbcmask = Samd21Instances.PM_BASE + Samd21PmLayout.APBCMASK_OFF
        Mmio.write32(apbcmask, Mmio.read32(apbcmask) | binding.apbcMask)
        let clkctrl = binding.gclkCoreId
            | (gclkGenerator << Samd21GclkLayout.CLKCTRL_GEN_LSB)
            | Samd21GclkLayout.CLKCTRL_CLKEN
        Mmio.write16(Samd21Instances.GCLK_BASE + Samd21GclkLayout.CLKCTRL_OFF,
                     UInt16(truncatingIfNeeded: clkctrl))
        while (Mmio.read8(Samd21Instances.GCLK_BASE + Samd21GclkLayout.STATUS_OFF)
               & UInt8(truncatingIfNeeded: Samd21GclkLayout.STATUS_SYNCBUSY)) != 0 {}

        // Pinmux: each signal's nibble is set by READ-MODIFY-WRITE of its PMUX byte, not by
        // writing the whole byte. Two of these signals can share one byte (an even/odd pin
        // pair) while a third sits in another PORT group entirely, and on this board's WINC
        // wiring they do -- writing whole bytes would have one signal clobber its neighbour.
        setMuxNibble(binding.pmuxMosiReg, binding.pmuxMosiShift)
        setMuxNibble(binding.pmuxSckReg, binding.pmuxSckShift)
        setMuxNibble(binding.pmuxMisoReg, binding.pmuxMisoShift)
        // DO and SCK are peripheral OUTPUTS and need only PMUXEN; DI is an input and
        // additionally needs INEN, or the SERCOM never sees the incoming line -- the same
        // gotcha the USART's RX pin has, for the same reason.
        Mmio.write8(binding.pincfgMosiReg, UInt8(truncatingIfNeeded: Samd21PortLayout.PINCFG0_PMUXEN))
        Mmio.write8(binding.pincfgSckReg, UInt8(truncatingIfNeeded: Samd21PortLayout.PINCFG0_PMUXEN))
        Mmio.write8(binding.pincfgMisoReg,
                    UInt8(truncatingIfNeeded: Samd21PortLayout.PINCFG0_PMUXEN
                                              | Samd21PortLayout.PINCFG0_INEN))

        // CTRLA (while disabled): SPI master, the binding's pad routing and clock mode,
        // MSB-first (DORD 0, so it contributes no bits). Then the plan-derived divisor, then
        // the receiver, then enable -- each synchronized write waiting its own bit.
        let ctrlaWord =
            (Samd21SercomSpiLayout.CTRLA_MODE_SPI_MASTER << Samd21SercomSpiLayout.CTRLA_MODE_LSB)
            | (binding.dopo << Samd21SercomSpiLayout.CTRLA_DOPO_LSB)
            | (binding.dipo << Samd21SercomSpiLayout.CTRLA_DIPO_LSB)
            | (binding.cpol << Samd21SercomSpiLayout.CTRLA_CPOL_LSB)
            | (binding.cpha << Samd21SercomSpiLayout.CTRLA_CPHA_LSB)
        Mmio.write32(ctrla, ctrlaWord)
        Mmio.write8(baud, UInt8(truncatingIfNeeded: binding.baudDivisor))
        Mmio.write32(ctrlb, Samd21SercomSpiLayout.CTRLB_RXEN)
        while (Mmio.read32(syncbusy) & Samd21SercomSpiLayout.SYNCBUSY_CTRLB) != 0 {}
        Mmio.write32(ctrla, Mmio.read32(ctrla) | Samd21SercomSpiLayout.CTRLA_ENABLE)
        while (Mmio.read32(syncbusy) & Samd21SercomSpiLayout.SYNCBUSY_ENABLE) != 0 {}
    }

    /// Sets ONE pin's mux nibble to the binding's function, leaving the byte's other nibble
    /// (its pin-pair partner) untouched.
    private func setMuxNibble(_ register: UInt32, _ shift: UInt32) {
        let current = UInt32(Mmio.read8(register))
        let cleared = current & ~(UInt32(0xF) << shift)
        Mmio.write8(register, UInt8(truncatingIfNeeded: cleared | (binding.pmuxFunc << shift)))
    }

    /// Clocks one byte out and the simultaneously received byte back -- the single primitive
    /// SPI actually has. Every other method here is written in terms of it.
    ///
    /// A master generates the clock, so a transfer always moves a byte in BOTH directions:
    /// a caller who only wants to send still receives (and may ignore) a byte, and a caller
    /// who only wants to receive must still send one. Bounded waits, so a dead bus yields
    /// rather than hanging the board.
    @discardableResult
    public func transfer(_ value: UInt8) -> UInt8 {
        for _ in 0 ..< 1_000_000 {
            if (Mmio.read8(intflag) & UInt8(truncatingIfNeeded: Samd21SercomSpiLayout.INTFLAG_DRE)) != 0 { break }
        }
        Mmio.write16(data, UInt16(value))
        for _ in 0 ..< 1_000_000 {
            if (Mmio.read8(intflag) & UInt8(truncatingIfNeeded: Samd21SercomSpiLayout.INTFLAG_RXC)) != 0 { break }
        }
        return UInt8(truncatingIfNeeded: Mmio.read16(data))
    }

    /// Clocks a caller-owned buffer out, discarding what arrives -- the write-only shape (the
    /// caller owns the buffer; the driver retains nothing past return; allocation-free).
    public func write(_ bytes: UnsafeBufferPointer<UInt8>) {
        for byte in bytes { transfer(byte) }
    }

    /// Fills a caller-owned buffer by clocking `padding` out for each byte wanted -- the
    /// read-only shape. `padding` is what the bus sees while the device replies; devices
    /// differ on what they want there, so the caller states it.
    public func read(into bytes: UnsafeMutableBufferPointer<UInt8>, padding: UInt8 = 0xFF) {
        for index in bytes.indices { bytes[index] = transfer(padding) }
    }

    /// Clocks `source` out while filling `destination` -- the full-duplex shape. Transfers
    /// `min(count)` bytes, so a caller cannot over-read a short buffer by mismatching lengths.
    public func transfer(_ source: UnsafeBufferPointer<UInt8>,
                         into destination: UnsafeMutableBufferPointer<UInt8>) {
        let count = min(source.count, destination.count)
        for index in 0 ..< count { destination[index] = transfer(source[index]) }
    }

    /// Waits (bounded) until the last byte has left the shift register (INTFLAG.TXC), so
    /// releasing a chip select never cuts a frame on the wire.
    public func flush() {
        for _ in 0 ..< 1_000_000 {
            if (Mmio.read8(intflag) & UInt8(truncatingIfNeeded: Samd21SercomSpiLayout.INTFLAG_TXC)) != 0 { return }
        }
    }

    /// True when the receiver has dropped a byte (STATUS.BUFOVF). Reading it does not clear
    /// it -- `clearOverflow()` does, so a caller can decide whether an overflow matters.
    public var overflowed: Bool {
        (Mmio.read16(status) & UInt16(truncatingIfNeeded: Samd21SercomSpiLayout.STATUS_BUFOVF)) != 0
    }

    /// Clears the receive-overflow flag (write-1-to-clear).
    public func clearOverflow() {
        Mmio.write16(status, UInt16(truncatingIfNeeded: Samd21SercomSpiLayout.STATUS_BUFOVF))
    }
}
