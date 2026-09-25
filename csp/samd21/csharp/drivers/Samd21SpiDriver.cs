// A Lamella.Hardware.SpiDriver master for the Microchip SAMD21's SERCOM in SPI mode: 8-bit frames,
// SPI modes 0 to 3, either bit order, and the rates the synchronous baud generator reaches from the
// binding's core clock -- half of it down to 1/512 of it.
//
// The pads arrive routed from a board's generated binding. The master drives no chip select of its
// own, so a non-negative SpiConnectionSettings.ChipSelectLine names the PORT pin -- PA00 to PA31 as
// 0 to 31, PB00 to PB31 as 32 to 63 -- that this driver asserts across each operation; -1 leaves
// selecting the device to the caller.
using System.Device.Gpio;
using System.Device.Spi;
using Lamella.Boards;
using Lamella.Generated;
using Lamella.Hardware;

public sealed class Samd21SpiDriver : SpiDriver
{
    private readonly uint _ctrla;
    private readonly uint _ctrlb;
    private readonly uint _baud;
    private readonly uint _intflag;
    private readonly uint _syncbusy;
    private readonly uint _data;
    private readonly Samd21SercomSpiBinding _binding;

    private int _chipSelectLine;
    private bool _chipSelectActiveHigh;
    private int _actualHz;

    const int Ok = 0;
    const int OtherError = 3;

    const int WaitBound = 100000;

    /// <summary>Binds the driver to one SPI wiring. No hardware is touched until
    /// <see cref="Configure"/>.</summary>
    public Samd21SpiDriver(Samd21SercomSpiBinding binding)
    {
        _binding = binding;
        uint sercom = binding.SercomBase;
        _ctrla = sercom + Samd21SercomSpiLayout.CTRLA_OFF;
        _ctrlb = sercom + Samd21SercomSpiLayout.CTRLB_OFF;
        _baud = sercom + Samd21SercomSpiLayout.BAUD_OFF;
        _intflag = sercom + Samd21SercomSpiLayout.INTFLAG_OFF;
        _syncbusy = sercom + Samd21SercomSpiLayout.SYNCBUSY_OFF;
        _data = sercom + Samd21SercomSpiLayout.DATA_OFF;
        _chipSelectLine = -1;
    }

    /// <summary>Brings the master up for <paramref name="settings"/>: the SERCOM's bus and core
    /// clocks on, a reset, the mode, pads and rate written while it is disabled, the pins routed,
    /// the receiver on, and ENABLE written last.</summary>
    /// <remarks>The clock is the fastest rate the baud generator reaches at or below the request:
    /// the binding's core clock divided by an even number from 2 to 512.
    /// <see cref="ActualClockFrequency"/> reports it.</remarks>
    /// <exception cref="System.ArgumentException">The frames are not 8 bits, the clock is below
    /// the core clock / 512, or the chip select line is past this part's two PORT groups.</exception>
    /// <exception cref="System.InvalidOperationException">The SERCOM's core clock could not be
    /// routed from its generator, or the SERCOM did not finish its reset or its enable, which is what
    /// an instance without a running core clock does.</exception>
    public override void Configure(SpiConnectionSettings settings)
    {
        if (settings.DataBitLength != 8)
        {
            BadArgument("this SPI master's frames are 8 bits");
        }
        int line = settings.ChipSelectLine;
        if (line >= 64)
        {
            BadArgument("the chip select line is past this part's two PORT groups");
        }
        uint baud = BaudFor(settings.ClockFrequency);

        uint ctrla = (Samd21SercomSpiLayout.CTRLA_MODE_SPI_MASTER << (int)Samd21SercomSpiLayout.CTRLA_MODE_LSB)
            | (_binding.Dopo << (int)Samd21SercomSpiLayout.CTRLA_DOPO_LSB)
            | (_binding.Dipo << (int)Samd21SercomSpiLayout.CTRLA_DIPO_LSB);
        if (settings.DataFlow == DataFlow.LsbFirst)
        {
            ctrla = ctrla | Samd21SercomSpiLayout.CTRLA_DORD;
        }
        if (settings.Mode == SpiMode.Mode1 || settings.Mode == SpiMode.Mode3)
        {
            ctrla = ctrla | Samd21SercomSpiLayout.CTRLA_CPHA;
        }
        if (settings.Mode == SpiMode.Mode2 || settings.Mode == SpiMode.Mode3)
        {
            ctrla = ctrla | Samd21SercomSpiLayout.CTRLA_CPOL;
        }

        // Clocks first: the APB gate, then the core clock, or every register below reads back as
        // whatever an unclocked peripheral returns. A core clock already running from another
        // generator is stopped before it moves (DS40001882D 15.6.3.3).
        uint apbcMask = Samd21Instances.PM_BASE + Samd21PmLayout.APBCMASK_OFF;
        Mmio.Write32(apbcMask, Mmio.Read32(apbcMask) | _binding.ApbcMask);
        if (!Samd21GenericClock.Route(_binding.GclkClkctrlValue))
        {
            Invalid("the SERCOM's core clock could not be routed from its generator");
        }

        // A reset, so a second Configure inherits nothing from the first. It also leaves the SERCOM
        // disabled, which the mode, pad, frame and rate registers need before they accept a write.
        Mmio.Write32(_ctrla, Samd21SercomSpiLayout.CTRLA_SWRST);
        if (!WaitSync(Samd21SercomSpiLayout.SYNCBUSY_SWRST))
        {
            Invalid("the SERCOM did not finish its reset; its core clock is not running");
        }
        Mmio.Write32(_ctrla, ctrla);
        // 8-bit frames, the chip select left to software, and the receiver on. Written while the
        // SERCOM is disabled, RXEN takes effect at once, with no synchronization to wait for.
        Mmio.Write32(_ctrlb, Samd21SercomSpiLayout.CTRLB_RXEN);
        Mmio.Write8(_baud, (byte)baud);

        // The pins: each signal's own PMUX nibble, leaving the other nibble of its byte as it was,
        // then PMUXEN. MISO is the input the master samples, so its pad's input buffer is enabled
        // as well.
        Route(_binding.PmuxMosiReg, _binding.PmuxMosiShift);
        Route(_binding.PmuxSckReg, _binding.PmuxSckShift);
        Route(_binding.PmuxMisoReg, _binding.PmuxMisoShift);
        Mmio.Write8(_binding.PincfgMosiReg, (byte)Samd21PortLayout.PINCFG0_PMUXEN);
        Mmio.Write8(_binding.PincfgSckReg, (byte)Samd21PortLayout.PINCFG0_PMUXEN);
        Mmio.Write8(_binding.PincfgMisoReg,
            (byte)(Samd21PortLayout.PINCFG0_PMUXEN | Samd21PortLayout.PINCFG0_INEN));

        _chipSelectLine = line;
        _chipSelectActiveHigh = settings.ChipSelectLineActiveState == PinValue.High;
        if (line >= 0)
        {
            // The select idles deasserted: its level is written before the pin becomes an output,
            // and the pad is under plain PORT control, its input buffer on so its level can be read.
            SetChipSelect(false);
            Mmio.Write8(PinCfgAddress(line), (byte)Samd21PortLayout.PINCFG0_INEN);
            Mmio.Write32(GroupBase(line) + Samd21PortLayout.DIRSET_OFF, PinMask(line));
        }

        Mmio.Write32(_ctrla, ctrla | Samd21SercomSpiLayout.CTRLA_ENABLE);
        if (!WaitSync(Samd21SercomSpiLayout.SYNCBUSY_ENABLE))
        {
            Invalid("the SERCOM did not finish enabling");
        }
    }

    /// <summary>Clocks <paramref name="count"/> bytes out while clocking as many in: an empty
    /// write buffer sends zeros, and an empty read buffer discards what arrives.</summary>
    /// <returns>0 when every byte completed; 3 when the SERCOM stopped raising its flags, which
    /// leaves the rest of the transfer unsent.</returns>
    public override int TransferFullDuplex(System.ReadOnlySpan<byte> writeBuffer,
                                           System.Span<byte> readBuffer, int count)
    {
        for (int i = 0; i < count; i++)
        {
            if (!WaitFlag(Samd21SercomSpiLayout.INTFLAG_DRE))
            {
                return OtherError;
            }
            uint tx = writeBuffer.IsEmpty ? 0u : (uint)writeBuffer[i];
            Mmio.Write16(_data, (ushort)tx);
            if (!WaitFlag(Samd21SercomSpiLayout.INTFLAG_RXC))
            {
                return OtherError;
            }
            // Reading DATA is what clears RXC.
            uint rx = Mmio.Read16(_data) & Samd21SercomSpiLayout.DATA_DATA;
            if (!readBuffer.IsEmpty)
            {
                readBuffer[i] = (byte)rx;
            }
        }
        return Ok;
    }

    /// <summary>Drives the chip select pin named by the settings; does nothing when none was
    /// named.</summary>
    public override void SetChipSelect(bool asserted)
    {
        if (_chipSelectLine < 0) return;
        uint offset = asserted == _chipSelectActiveHigh
            ? Samd21PortLayout.OUTSET_OFF
            : Samd21PortLayout.OUTCLR_OFF;
        Mmio.Write32(GroupBase(_chipSelectLine) + offset, PinMask(_chipSelectLine));
    }

    /// <summary>The bit rate the master runs at: the fastest rate the baud generator reaches at or
    /// below the request.</summary>
    public override int ActualClockFrequency { get { return _actualHz; } }

    // f = core / (2 * (BAUD + 1)) (Table 25-2), so the divisor 2 * (BAUD + 1) is rounded UP from
    // core / request, and the rate the master runs at is never above the one asked for.
    uint BaudFor(int hz)
    {
        if (hz <= 0)
        {
            BadArgument("the SPI clock must be above zero");
        }
        uint core = _binding.CoreClockHz;
        uint half = core / 2u;
        if ((uint)hz >= half)
        {
            _actualHz = (int)half;
            return 0u;
        }
        uint twice = (uint)hz * 2u;
        uint steps = (core + twice - 1u) / twice;
        if (steps > 256u)
        {
            BadArgument("the slowest rate this SPI master runs at is its core clock / 512");
        }
        _actualHz = (int)(core / (2u * steps));
        return steps - 1u;
    }

    void Route(uint pmuxReg, uint shift)
    {
        uint nibble = Samd21PortLayout.PMUX0_PMUXE << (int)shift;
        uint kept = Mmio.Read8(pmuxReg) & ~nibble;
        Mmio.Write8(pmuxReg, (byte)(kept | (_binding.PmuxFunc << (int)shift)));
    }

    bool WaitSync(uint bits)
    {
        for (int polls = 0; polls < WaitBound; polls = polls + 1)
        {
            if ((Mmio.Read32(_syncbusy) & bits) == 0u) return true;
        }
        return false;
    }

    bool WaitFlag(uint flag)
    {
        for (int polls = 0; polls < WaitBound; polls = polls + 1)
        {
            if ((Mmio.Read8(_intflag) & flag) != 0u) return true;
        }
        return false;
    }

    static uint GroupBase(int line)
    {
        return line < 32 ? Samd21Instances.PORTA_BASE : Samd21Instances.PORTB_BASE;
    }

    static uint PinMask(int line)
    {
        return 1u << (line & 31);
    }

    static uint PinCfgAddress(int line)
    {
        return GroupBase(line) + Samd21PortLayout.PINCFG0_OFF + (uint)(line & 31);
    }

    static void BadArgument(string why)
    {
#if LAMELLA_CORLIB_LINKED
        throw new System.ArgumentException(why);
#else
        throw new System.Exception(why);
#endif
    }

    static void Invalid(string why)
    {
#if LAMELLA_CORLIB_LINKED
        throw new System.InvalidOperationException(why);
#else
        throw new System.Exception(why);
#endif
    }
}
