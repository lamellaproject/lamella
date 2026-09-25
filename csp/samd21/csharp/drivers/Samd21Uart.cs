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
    /// <exception cref="System.InvalidOperationException">The SERCOM's core clock could not be
    /// routed from its generator, or the SERCOM did not finish synchronizing its setup.</exception>
    public void Init()
    {
        // The instance's bus clock, then its core clock. A core clock already running from another
        // generator -- the resident firmware's, say -- is stopped before it moves (DS40001882D
        // 15.6.3.3).
        uint apbcmask = Samd21Instances.PM_BASE + Samd21PmLayout.APBCMASK_OFF;
        Mmio.Write32(apbcmask, Mmio.Read32(apbcmask) | _binding.ApbcMask);
        if (!Samd21GenericClock.Route(_binding.GclkClkctrlValue))
        {
            Invalid("the SERCOM's core clock could not be routed from its generator");
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
        WaitSync(Samd21SercomUsartLayout.SYNCBUSY_CTRLB);
        Mmio.Write32(_ctrla, Mmio.Read32(_ctrla) | Samd21SercomUsartLayout.CTRLA_ENABLE);
        WaitSync(Samd21SercomUsartLayout.SYNCBUSY_ENABLE);
    }

    // A bound on each synchronization wait, so a SERCOM that never finishes is reported rather than
    // waited on forever.
    const int WaitBound = 100000;

    void WaitSync(uint bits)
    {
        for (int spin = 0; spin < WaitBound; spin++)
        {
            if ((Mmio.Read32(_syncbusy) & bits) == 0u)
            {
                return;
            }
        }
        Invalid("the SERCOM did not finish synchronizing its setup");
    }

    // The corlib-free tiers bind only System.Exception's constructor, so the precise type is chosen
    // where a corlib is linked, as the family's other drivers do.
    static void Invalid(string why)
    {
#if LAMELLA_CORLIB_LINKED
        throw new System.InvalidOperationException(why);
#else
        throw new System.Exception(why);
#endif
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
