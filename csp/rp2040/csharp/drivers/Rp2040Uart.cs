// The RP2040 PL011-UART driver, in C# over Lamella.Hardware.Mmio
using Lamella.Boards;
using Lamella.Generated;
using Lamella.Hardware;

public sealed class Rp2040Uart
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
    private readonly uint _resetsClr;
    private readonly uint _resetsDone;
    private readonly Rp2040UartBinding _binding;

    /// <summary>Binds the driver to one PL011 wiring; no hardware is touched until
    /// <see cref="Init"/>.</summary>
    public Rp2040Uart(Rp2040UartBinding binding)
    {
        _binding = binding;
        _dr = binding.UartBase + Rp2040UartLayout.UARTDR_OFF;
        _fr = binding.UartBase + Rp2040UartLayout.UARTFR_OFF;
        _ibrd = binding.UartBase + Rp2040UartLayout.UARTIBRD_OFF;
        _fbrd = binding.UartBase + Rp2040UartLayout.UARTFBRD_OFF;
        _lcrh = binding.UartBase + Rp2040UartLayout.UARTLCR_H_OFF;
        _cr = binding.UartBase + Rp2040UartLayout.UARTCR_OFF;
        _xoscCtrl = Rp2040Instances.XOSC_BASE + Rp2040XoscLayout.CTRL_OFF;
        _xoscStatus = Rp2040Instances.XOSC_BASE + Rp2040XoscLayout.STATUS_OFF;
        _xoscStartup = Rp2040Instances.XOSC_BASE + Rp2040XoscLayout.STARTUP_OFF;
        _clkPeriCtrl = Rp2040Instances.CLOCKS_BASE + Rp2040ClocksLayout.CLK_PERI_CTRL_OFF;
        _resetsClr = Rp2040Instances.RESETS_CLR_BASE + Rp2040ResetsLayout.RESET_OFF;
        _resetsDone = Rp2040Instances.RESETS_BASE + Rp2040ResetsLayout.RESET_DONE_OFF;
    }

    /// <summary>Brings the bound PL011 up at <paramref name="baud"/>, 8N1, clocked from the
    /// crystal: starts the XOSC (idempotent when already running), switches clk_peri to it
    /// (disable, select, re-enable -- no glitchless mux), releases the binding's reset set,
    /// routes both pins' function select, then programs the divisor and enables (IBRD, FBRD,
    /// then LCR_H -- the latching order).</summary>
    public void Init(int baud)
    {
        Mmio.Write32(_xoscStartup, Rp2040XoscLayout.STARTUP_DELAY_1MS);
        Mmio.Write32(_xoscCtrl,
            (Rp2040XoscLayout.CTRL_ENABLE_MAGIC << (int)Rp2040XoscLayout.CTRL_ENABLE_LSB)
            | Rp2040XoscLayout.CTRL_FREQ_RANGE_1_15MHZ);
        for (int spin = 0; spin < 100000; spin++)
        {
            if ((Mmio.Read32(_xoscStatus) & Rp2040XoscLayout.STATUS_STABLE) != 0u) break;
        }

        uint periSelected = Rp2040ClocksLayout.AUXSRC_XOSC << (int)Rp2040ClocksLayout.CLK_PERI_CTRL_AUXSRC_LSB;
        Mmio.Write32(_clkPeriCtrl, 0);
        Mmio.Write32(_clkPeriCtrl, periSelected);
        Mmio.Write32(_clkPeriCtrl, periSelected | Rp2040ClocksLayout.CLK_PERI_CTRL_ENABLE);

        Mmio.Write32(_resetsClr, _binding.ResetMask);
        for (int spin = 0; spin < 100000; spin++)
        {
            if ((Mmio.Read32(_resetsDone) & _binding.ResetMask) == _binding.ResetMask) break;
        }

        Mmio.Write32(_binding.IoTxCtrl, _binding.Funcsel);
        Mmio.Write32(_binding.IoRxCtrl, _binding.Funcsel);

        uint sixtyFourths = (_binding.ClkPeriHz * 4u + (uint)(baud / 2)) / (uint)baud;
        Mmio.Write32(_ibrd, sixtyFourths >> 6);
        Mmio.Write32(_fbrd, sixtyFourths & Rp2040UartLayout.UARTFBRD_BAUD_DIVFRAC);
        Mmio.Write32(_lcrh,
            (Rp2040UartLayout.WLEN_8BIT << (int)Rp2040UartLayout.UARTLCR_H_WLEN_LSB)
            | Rp2040UartLayout.UARTLCR_H_FEN);
        Mmio.Write32(_cr, Rp2040UartLayout.UARTCR_UARTEN
            | Rp2040UartLayout.UARTCR_TXE | Rp2040UartLayout.UARTCR_RXE);
    }

    /// <summary>Ties UARTTXD back to UARTRXD inside the PL011 (the no-hands self-test). The
    /// control register is reprogrammed disabled, per the PL011 sequence: drain, disable,
    /// rewrite.</summary>
    public void EnableLoopback(bool enabled)
    {
        Flush();
        uint enable = Rp2040UartLayout.UARTCR_UARTEN
            | Rp2040UartLayout.UARTCR_TXE | Rp2040UartLayout.UARTCR_RXE;
        Mmio.Write32(_cr, 0);
        Mmio.Write32(_cr, enabled ? (enable | Rp2040UartLayout.UARTCR_LBE) : enable);
    }

    /// <summary>Sends one byte, waiting for FIFO room first.</summary>
    public void WriteByte(int value)
    {
        for (int spin = 0; spin < 100000; spin++)
        {
            if ((Mmio.Read32(_fr) & Rp2040UartLayout.UARTFR_TXFF) == 0u) break;
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
            if ((Mmio.Read32(_fr) & Rp2040UartLayout.UARTFR_BUSY) == 0u) return;
        }
    }

    /// <summary>1 when at least one received byte waits (the PL011 exposes flags, not a count).</summary>
    public int Available
    {
        get { return (Mmio.Read32(_fr) & Rp2040UartLayout.UARTFR_RXFE) == 0u ? 1 : 0; }
    }

    /// <summary>Pops one received byte, or -1 when the RX FIFO is empty (the Stream convention).</summary>
    public int ReadByte()
    {
        if ((Mmio.Read32(_fr) & Rp2040UartLayout.UARTFR_RXFE) != 0u) return -1;
        return (int)(Mmio.Read32(_dr) & Rp2040UartLayout.UARTDR_DATA);
    }
}
