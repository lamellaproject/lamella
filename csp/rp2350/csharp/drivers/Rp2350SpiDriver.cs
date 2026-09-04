// A Lamella.Hardware.SpiDriver for the RP2350's ARM PrimeCell PL022 SSP, over
using System;
using System.Device.Gpio;
using System.Device.Spi;
using Lamella.Boards;
using Lamella.Generated;
using Lamella.Hardware;

public sealed class Rp2350SpiDriver : SpiDriver
{
    private readonly uint _cr0;
    private readonly uint _cr1;
    private readonly uint _dr;
    private readonly uint _sr;
    private readonly uint _cpsr;
    private readonly uint _xoscCtrl;
    private readonly uint _xoscStatus;
    private readonly uint _xoscStartup;
    private readonly uint _clkPeriCtrl;
    private readonly uint _clkPeriDiv;
    private readonly uint _resetsClr;
    private readonly uint _resetsDone;
    private readonly uint _sioOutSet;
    private readonly uint _sioOutClr;
    private readonly uint _sioOeSet;
    private readonly uint _ioCtrl0;
    private readonly uint _pads0;
    private readonly uint _padIe;
    private readonly Rp2350SpiBinding _binding;

    private int _chipSelectLine;
    private bool _chipSelectActiveHigh;
    private int _actualHz;

    /// <summary>Binds the driver to one PL022 wiring; no hardware is touched until
    /// <see cref="Configure"/>.</summary>
    public Rp2350SpiDriver(Rp2350SpiBinding binding)
    {
        _binding = binding;
        _cr0 = binding.SspBase + Rp2350SpiLayout.SSPCR0_OFF;
        _cr1 = binding.SspBase + Rp2350SpiLayout.SSPCR1_OFF;
        _dr = binding.SspBase + Rp2350SpiLayout.SSPDR_OFF;
        _sr = binding.SspBase + Rp2350SpiLayout.SSPSR_OFF;
        _cpsr = binding.SspBase + Rp2350SpiLayout.SSPCPSR_OFF;
        _xoscCtrl = Rp2350Instances.XOSC_BASE + Rp2350XoscLayout.CTRL_OFF;
        _xoscStatus = Rp2350Instances.XOSC_BASE + Rp2350XoscLayout.STATUS_OFF;
        _xoscStartup = Rp2350Instances.XOSC_BASE + Rp2350XoscLayout.STARTUP_OFF;
        _clkPeriCtrl = Rp2350Instances.CLOCKS_BASE + Rp2350ClocksLayout.CLK_PERI_CTRL_OFF;
        _clkPeriDiv = Rp2350Instances.CLOCKS_BASE + Rp2350ClocksLayout.CLK_PERI_DIV_OFF;
        _resetsClr = Rp2350Instances.RESETS_CLR_BASE + Rp2350ResetsLayout.RESET_OFF;
        _resetsDone = Rp2350Instances.RESETS_BASE + Rp2350ResetsLayout.RESET_DONE_OFF;
        _sioOutSet = Rp2350Instances.SIO_BASE + Rp2350SioLayout.GPIO_OUT_SET_OFF;
        _sioOutClr = Rp2350Instances.SIO_BASE + Rp2350SioLayout.GPIO_OUT_CLR_OFF;
        _sioOeSet = Rp2350Instances.SIO_BASE + Rp2350SioLayout.GPIO_OE_SET_OFF;
        _ioCtrl0 = Rp2350Instances.IO_BANK0_BASE + Rp2350IoBank0Layout.GPIO0_CTRL_OFF;
        _pads0 = Rp2350Instances.PADS_BANK0_BASE + Rp2350PadsBank0Layout.GPIO0_OFF;
        _padIe = Rp2350PadsBank0Layout.GPIO0_IE;
    }

    /// <summary>Executes the proven init for the requested settings: crystal-exact clk_peri
    /// (the UART's sequence), the reset release, pad de-isolation, pin routing, then the
    /// even-prescaler divisor with the port disabled (SSE last). Envelope rejects are LOUD:
    /// the PL022 is MSB-first only, and this driver speaks 8-bit frames.</summary>
    public override void Configure(SpiConnectionSettings settings)
    {
        if (settings.DataFlow != System.Device.Spi.DataFlow.MsbFirst)
        {
            throw new ArgumentException("pl022 transfers msb-first only");
        }
        if (settings.DataBitLength != 8)
        {
            throw new ArgumentException("this driver speaks 8-bit frames");
        }

        Mmio.Write32(_xoscStartup, Rp2350XoscLayout.STARTUP_DELAY_1MS);
        Mmio.Write32(_xoscCtrl,
            (Rp2350XoscLayout.CTRL_ENABLE_MAGIC << (int)Rp2350XoscLayout.CTRL_ENABLE_LSB)
            | Rp2350XoscLayout.CTRL_FREQ_RANGE_1_15MHZ);
        for (int spin = 0; spin < 100000; spin++)
        {
            if ((Mmio.Read32(_xoscStatus) & Rp2350XoscLayout.STATUS_STABLE) != 0u) break;
        }
        uint periSelected = Rp2350ClocksLayout.CLK_PERI_AUXSRC_XOSC << (int)Rp2350ClocksLayout.CLK_PERI_CTRL_AUXSRC_LSB;
        Mmio.Write32(_clkPeriCtrl, 0);
        Mmio.Write32(_clkPeriCtrl, periSelected);
        Mmio.Write32(_clkPeriDiv, 1u << (int)Rp2350ClocksLayout.CLK_PERI_DIV_INT_LSB);
        Mmio.Write32(_clkPeriCtrl, periSelected | Rp2350ClocksLayout.CLK_PERI_CTRL_ENABLE);

        Mmio.Write32(_resetsClr, _binding.ResetMask);
        for (int spin = 0; spin < 100000; spin++)
        {
            if ((Mmio.Read32(_resetsDone) & _binding.ResetMask) == _binding.ResetMask) break;
        }

        Mmio.Write32(_binding.PadsMiso, _padIe);
        Mmio.Write32(_binding.PadsSck, _padIe);
        Mmio.Write32(_binding.PadsMosi, _padIe);
        Mmio.Write32(_binding.IoMisoCtrl, _binding.Funcsel);
        Mmio.Write32(_binding.IoSckCtrl, _binding.Funcsel);
        Mmio.Write32(_binding.IoMosiCtrl, _binding.Funcsel);

        _chipSelectLine = settings.ChipSelectLine;
        _chipSelectActiveHigh = settings.ChipSelectLineActiveState == PinValue.High;
        if (_chipSelectLine < 0)
        {
            Mmio.Write32(_binding.PadsCs, _padIe);
            Mmio.Write32(_binding.IoCsCtrl, _binding.Funcsel);
        }
        else
        {
            uint pin = (uint)_chipSelectLine;
            Mmio.Write32(_pads0 + Rp2350PadsBank0Layout.GPIO_STRIDE * pin, _padIe);
            Mmio.Write32(_ioCtrl0 + Rp2350IoBank0Layout.GPIO_CTRL_STRIDE * pin, Rp2350IoBank0Layout.FUNCSEL_SIO);
            SetChipSelect(false);
            Mmio.Write32(_sioOeSet, 1u << _chipSelectLine);
        }

        uint sspclk = _binding.SspclkHz;
        int clockHz = settings.ClockFrequency;
        uint prescale = 2;
        while (prescale < 254 && sspclk / (prescale * 256u) > (uint)clockHz)
        {
            prescale += 2;
        }
        uint divisor = (sspclk + (uint)(prescale * clockHz) - 1) / (prescale * (uint)clockHz);
        if (divisor < 1) divisor = 1;
        uint scr = divisor - 1;
        _actualHz = (int)(sspclk / (prescale * (scr + 1u)));

        uint mode = 0;
        if (settings.Mode == SpiMode.Mode1) mode = Rp2350SpiLayout.SSPCR0_SPH;
        else if (settings.Mode == SpiMode.Mode2) mode = Rp2350SpiLayout.SSPCR0_SPO;
        else if (settings.Mode == SpiMode.Mode3) mode = Rp2350SpiLayout.SSPCR0_SPO | Rp2350SpiLayout.SSPCR0_SPH;

        Mmio.Write32(_cr1, 0);
        Mmio.Write32(_cpsr, prescale);
        Mmio.Write32(_cr0, Rp2350SpiLayout.DSS_8BIT | mode | (scr << (int)Rp2350SpiLayout.SSPCR0_SCR_LSB));
        Mmio.Write32(_cr1, Rp2350SpiLayout.SSPCR1_SSE);
    }

    /// <summary>The whole-burst full-duplex primitive: each byte transmits (empty = zeros) while
    /// its echo lands (empty = discarded). The PL022 reports no per-transfer errors; 0 = done.</summary>
    public override int TransferFullDuplex(System.ReadOnlySpan<byte> writeBuffer,
                                           System.Span<byte> readBuffer, int count)
    {
        for (int i = 0; i < count; i++)
        {
            int tx = !writeBuffer.IsEmpty ? writeBuffer[i] : 0;
            int rx = TransferByte(tx);
            if (!readBuffer.IsEmpty) readBuffer[i] = (byte)rx;
        }
        return 0;
    }

    /// <summary>Drives the managed chip-select line; a no-op on the raw bus. Deassertion
    /// waits for the shift register to go idle first, so CS never releases mid-frame.</summary>
    public override void SetChipSelect(bool asserted)
    {
        if (_chipSelectLine < 0) return;
        if (!asserted) Flush();
        bool driveHigh = asserted == _chipSelectActiveHigh;
        Mmio.Write32(driveHigh ? _sioOutSet : _sioOutClr, 1u << _chipSelectLine);
    }

    /// <summary>The realized bit rate: at or below the request, never above.</summary>
    public override int ActualClockFrequency { get { return _actualHz; } }

    /// <summary>Ties the TX shifter to the RX shifter inside the PL022 (the no-hands
    /// full-duplex self-test); reprogrammed with the port disabled. Layer-1 test hook.</summary>
    public void EnableLoopback(bool enabled)
    {
        Flush();
        uint enable = Rp2350SpiLayout.SSPCR1_SSE;
        Mmio.Write32(_cr1, 0);
        Mmio.Write32(_cr1, enabled ? (enable | Rp2350SpiLayout.SSPCR1_LBM) : enable);
    }

    /// <summary>One full-duplex 8-bit transfer: sends <paramref name="value"/>, returns the
    /// byte clocked back in.</summary>
    public int TransferByte(int value)
    {
        for (int spin = 0; spin < 100000; spin++)
        {
            if ((Mmio.Read32(_sr) & Rp2350SpiLayout.SSPSR_TNF) != 0u) break;
        }
        Mmio.Write32(_dr, (uint)(value & 0xFF));
        for (int spin = 0; spin < 100000; spin++)
        {
            if ((Mmio.Read32(_sr) & Rp2350SpiLayout.SSPSR_RNE) != 0u) break;
        }
        return (int)(Mmio.Read32(_dr) & 0xFFu);
    }

    /// <summary>Waits until the shift register is idle (SSPSR.BSY covers the frame on the wire).</summary>
    public void Flush()
    {
        for (int spin = 0; spin < 100000; spin++)
        {
            if ((Mmio.Read32(_sr) & Rp2350SpiLayout.SSPSR_BSY) == 0u) return;
        }
    }

    /// <summary>Discards anything left in the RX FIFO (e.g. before a compare loop).</summary>
    public void DrainReceive()
    {
        for (int spin = 0; spin < 100000; spin++)
        {
            if ((Mmio.Read32(_sr) & Rp2350SpiLayout.SSPSR_RNE) == 0u) return;
            Mmio.Read32(_dr);
        }
    }
}
