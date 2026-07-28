// The SAMD21 SERCOM-USART driver, in C# over Lamella.Hardware.Mmio -- ONE driver for every
using Lamella.Boards;
using Lamella.Generated;
using Lamella.Hardware;

public sealed class Samd21Uart
{
    private readonly uint _ctrla;
    private readonly uint _ctrlb;
    private readonly uint _baud;
    private readonly uint _intflag;
    private readonly uint _syncbusy;
    private readonly uint _data;
    private readonly Samd21SercomUsartBinding _binding;

    /// <summary>Binds the driver to one SERCOM-USART wiring; no hardware is touched until
    /// <see cref="Init"/>.</summary>
    public Samd21Uart(Samd21SercomUsartBinding binding)
    {
        _binding = binding;
        _ctrla = binding.SercomBase + Samd21SercomUsartLayout.CTRLA_OFF;
        _ctrlb = binding.SercomBase + Samd21SercomUsartLayout.CTRLB_OFF;
        _baud = binding.SercomBase + Samd21SercomUsartLayout.BAUD_OFF;
        _intflag = binding.SercomBase + Samd21SercomUsartLayout.INTFLAG_OFF;
        _syncbusy = binding.SercomBase + Samd21SercomUsartLayout.SYNCBUSY_OFF;
        _data = binding.SercomBase + Samd21SercomUsartLayout.DATA_OFF;
    }

    /// <summary>Brings the bound SERCOM up as a USART at the binding's rate, 8N1 LSB-first:
    /// gates the APB clock, routes the core clock per the binding's composed GCLK word, muxes
    /// the TX/RX pins (RX input buffer ON), configures, then enables -- each enable-protected
    /// write waiting out its SYNCBUSY bit. Idempotent -- safe over a SERCOM the resident
    /// firmware already configured.</summary>
    public void Init()
    {
        uint apbcmask = Samd21Instances.PM_BASE + Samd21PmLayout.APBCMASK_OFF;
        Mmio.Write32(apbcmask, Mmio.Read32(apbcmask) | _binding.ApbcMask);
        Mmio.Write16(Samd21Instances.GCLK_BASE + Samd21GclkLayout.CLKCTRL_OFF,
            (ushort)_binding.GclkClkctrlValue);
        while ((Mmio.Read8(Samd21Instances.GCLK_BASE + Samd21GclkLayout.STATUS_OFF)
            & (byte)Samd21GclkLayout.STATUS_SYNCBUSY) != 0)
        {
        }

        Mmio.Write8(_binding.PmuxReg, (byte)_binding.PmuxPair);
        Mmio.Write8(_binding.PincfgTxReg, (byte)Samd21PortLayout.PINCFG0_PMUXEN);
        Mmio.Write8(_binding.PincfgRxReg,
            (byte)(Samd21PortLayout.PINCFG0_PMUXEN | Samd21PortLayout.PINCFG0_INEN));

        uint ctrla = (Samd21SercomUsartLayout.CTRLA_MODE_USART_INTCLK
                << (int)Samd21SercomUsartLayout.CTRLA_MODE_LSB)
            | Samd21SercomUsartLayout.CTRLA_RUNSTDBY
            | (_binding.Txpo << (int)Samd21SercomUsartLayout.CTRLA_TXPO_LSB)
            | (_binding.Rxpo << (int)Samd21SercomUsartLayout.CTRLA_RXPO_LSB)
            | Samd21SercomUsartLayout.CTRLA_DORD;
        Mmio.Write32(_ctrla, ctrla);
        Mmio.Write16(_baud, (ushort)_binding.BaudDivisor);
        Mmio.Write32(_ctrlb,
            Samd21SercomUsartLayout.CTRLB_TXEN | Samd21SercomUsartLayout.CTRLB_RXEN);
        while ((Mmio.Read32(_syncbusy) & Samd21SercomUsartLayout.SYNCBUSY_CTRLB) != 0u)
        {
        }
        Mmio.Write32(_ctrla, Mmio.Read32(_ctrla) | Samd21SercomUsartLayout.CTRLA_ENABLE);
        while ((Mmio.Read32(_syncbusy) & Samd21SercomUsartLayout.SYNCBUSY_ENABLE) != 0u)
        {
        }
    }

    /// <summary>Sends one byte, waiting (bounded) for the Data Register Empty flag first.</summary>
    public void WriteByte(int value)
    {
        for (int spin = 0; spin < 1000000; spin++)
        {
            if ((Mmio.Read8(_intflag) & (byte)Samd21SercomUsartLayout.INTFLAG_DRE) != 0) break;
        }
        Mmio.Write16(_data, (ushort)(value & 0xFF));
    }

    /// <summary>Sends a string as its low-byte (ASCII) characters.</summary>
    public void Write(string text)
    {
        for (int i = 0; i < text.Length; i++)
        {
            WriteByte(text[i]);
        }
    }

    /// <summary>Waits (bounded) until the last frame has left the shift register
    /// (INTFLAG.TXC), so a mode switch or a caller's delay never cuts a frame on the wire.</summary>
    public void Flush()
    {
        for (int spin = 0; spin < 1000000; spin++)
        {
            if ((Mmio.Read8(_intflag) & (byte)Samd21SercomUsartLayout.INTFLAG_TXC) != 0) return;
        }
    }

    /// <summary>1 when at least one received byte waits (INTFLAG.RXC), else 0.</summary>
    public int Available
    {
        get
        {
            return (Mmio.Read8(_intflag) & (byte)Samd21SercomUsartLayout.INTFLAG_RXC) != 0 ? 1 : 0;
        }
    }

    /// <summary>Pops one received byte (the DATA field is 9 bits wide), or -1 when the RX
    /// register is empty (the Stream convention).</summary>
    public int ReadByte()
    {
        if ((Mmio.Read8(_intflag) & (byte)Samd21SercomUsartLayout.INTFLAG_RXC) == 0) return -1;
        int raw = Mmio.Read16(_data);
        return raw & (int)Samd21SercomUsartLayout.DATA_DATA;
    }
}
