// The RP2350 PL011-UART driver, in C# over Lamella.Hardware.Mmio -- ONE driver for every
using Lamella.Boards;
using Lamella.Generated;
using Lamella.Hardware;

public sealed class Rp2350Uart
{
    private readonly uint _dr;
    private readonly uint _fr;
    private readonly uint _ibrd;
    private readonly uint _fbrd;
    private readonly uint _lcrh;
    private readonly uint _cr;
    private readonly uint _xoscCtrl;
    private readonly uint _xoscStatus;
    private readonly uint _xoscStartup;
    private readonly uint _clkPeriCtrl;
    private readonly uint _clkPeriDiv;
    private readonly uint _resetsClr;
    private readonly uint _resetsDone;
    private readonly uint _padIe;
    private readonly Rp2350UartBinding _binding;

    /// <summary>Binds the driver to one PL011 wiring; no hardware is touched until
    /// <see cref="Init"/>.</summary>
    public Rp2350Uart(Rp2350UartBinding binding)
    {
        _binding = binding;
        _dr = binding.UartBase + Rp2350UartLayout.UARTDR_OFF;
        _fr = binding.UartBase + Rp2350UartLayout.UARTFR_OFF;
        _ibrd = binding.UartBase + Rp2350UartLayout.UARTIBRD_OFF;
        _fbrd = binding.UartBase + Rp2350UartLayout.UARTFBRD_OFF;
        _lcrh = binding.UartBase + Rp2350UartLayout.UARTLCR_H_OFF;
        _cr = binding.UartBase + Rp2350UartLayout.UARTCR_OFF;
        _xoscCtrl = Rp2350Instances.XOSC_BASE + Rp2350XoscLayout.CTRL_OFF;
        _xoscStatus = Rp2350Instances.XOSC_BASE + Rp2350XoscLayout.STATUS_OFF;
        _xoscStartup = Rp2350Instances.XOSC_BASE + Rp2350XoscLayout.STARTUP_OFF;
        _clkPeriCtrl = Rp2350Instances.CLOCKS_BASE + Rp2350ClocksLayout.CLK_PERI_CTRL_OFF;
        _clkPeriDiv = Rp2350Instances.CLOCKS_BASE + Rp2350ClocksLayout.CLK_PERI_DIV_OFF;
        _resetsClr = Rp2350Instances.RESETS_CLR_BASE + Rp2350ResetsLayout.RESET_OFF;
        _resetsDone = Rp2350Instances.RESETS_BASE + Rp2350ResetsLayout.RESET_DONE_OFF;
        _padIe = Rp2350PadsBank0Layout.GPIO0_IE;
    }

    /// <summary>Brings the bound PL011 up at <paramref name="baud"/>, 8N1, clocked from the
    /// crystal: starts the XOSC (idempotent when already running), switches clk_peri to it
    /// (disable, select, divide-by-1 stated explicitly, re-enable), releases the binding's
    /// reset set, de-isolates both pads (the RP2350 delta), routes both pins' function
    /// select, then programs the divisor and enables (IBRD, FBRD, then LCR_H -- the latching
    /// order).</summary>
    public void Init(int baud)
    {
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

        Mmio.Write32(_binding.PadsTx, _padIe);
        Mmio.Write32(_binding.PadsRx, _padIe);
        Mmio.Write32(_binding.IoTxCtrl, _binding.Funcsel);
        Mmio.Write32(_binding.IoRxCtrl, _binding.Funcsel);

        uint sixtyFourths = (_binding.ClkPeriHz * 4u + (uint)(baud / 2)) / (uint)baud;
        Mmio.Write32(_ibrd, sixtyFourths >> 6);
        Mmio.Write32(_fbrd, sixtyFourths & Rp2350UartLayout.UARTFBRD_BAUD_DIVFRAC);
        Mmio.Write32(_lcrh,
            (Rp2350UartLayout.WLEN_8BIT << (int)Rp2350UartLayout.UARTLCR_H_WLEN_LSB)
            | Rp2350UartLayout.UARTLCR_H_FEN);
        Mmio.Write32(_cr, Rp2350UartLayout.UARTCR_UARTEN
            | Rp2350UartLayout.UARTCR_TXE | Rp2350UartLayout.UARTCR_RXE);
    }

    /// <summary>Ties UARTTXD back to UARTRXD inside the PL011 (the no-hands self-test). The
    /// control register is reprogrammed disabled, per the PL011 sequence: drain, disable,
    /// rewrite.</summary>
    public void EnableLoopback(bool enabled)
    {
        Flush();
        uint enable = Rp2350UartLayout.UARTCR_UARTEN
            | Rp2350UartLayout.UARTCR_TXE | Rp2350UartLayout.UARTCR_RXE;
        Mmio.Write32(_cr, 0);
        Mmio.Write32(_cr, enabled ? (enable | Rp2350UartLayout.UARTCR_LBE) : enable);
    }

    /// <summary>Sends one byte, waiting for FIFO room first.</summary>
    public void WriteByte(int value)
    {
        for (int spin = 0; spin < 100000; spin++)
        {
            if ((Mmio.Read32(_fr) & Rp2350UartLayout.UARTFR_TXFF) == 0u) break;
        }
        Mmio.Write32(_dr, (uint)(value & 0xFF));
    }

    /// <summary>Sends a string as its low-byte (ASCII) characters.</summary>
    public void Write(string text)
    {
        for (int i = 0; i < text.Length; i++)
        {
            WriteByte(text[i]);
        }
    }

    /// <summary>Waits until the FIFO AND the shift register are drained (FR.BUSY covers the
    /// last frame on the wire), so mode switches never cut a frame.</summary>
    public void Flush()
    {
        for (int spin = 0; spin < 100000; spin++)
        {
            if ((Mmio.Read32(_fr) & Rp2350UartLayout.UARTFR_BUSY) == 0u) return;
        }
    }

    /// <summary>1 when at least one received byte waits (the PL011 exposes flags, not a count).</summary>
    public int Available
    {
        get { return (Mmio.Read32(_fr) & Rp2350UartLayout.UARTFR_RXFE) == 0u ? 1 : 0; }
    }

    /// <summary>Pops one received byte, or -1 when the RX FIFO is empty (the Stream convention).</summary>
    public int ReadByte()
    {
        if ((Mmio.Read32(_fr) & Rp2350UartLayout.UARTFR_RXFE) != 0u) return -1;
        return (int)(Mmio.Read32(_dr) & Rp2350UartLayout.UARTDR_DATA);
    }
}
