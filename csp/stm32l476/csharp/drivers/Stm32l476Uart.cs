// The STM32L476 USART driver, in C# over Lamella.Hardware.Mmio -- ONE driver for every USART
using Lamella.Boards;
using Lamella.Generated;
using Lamella.Hardware;

public sealed class Stm32l476Uart
{
    private readonly uint _cr1;
    private readonly uint _brr;
    private readonly uint _isr;
    private readonly uint _icr;
    private readonly uint _rdr;
    private readonly uint _tdr;
    private readonly Stm32l476UsartBinding _binding;

    /// <summary>Binds the driver to one USART wiring; no hardware is touched until
    /// <see cref="Init"/>.</summary>
    public Stm32l476Uart(Stm32l476UsartBinding binding)
    {
        _binding = binding;
        _cr1 = binding.UsartBase + Stm32l476UsartLayout.CR1_OFF;
        _brr = binding.UsartBase + Stm32l476UsartLayout.BRR_OFF;
        _isr = binding.UsartBase + Stm32l476UsartLayout.ISR_OFF;
        _icr = binding.UsartBase + Stm32l476UsartLayout.ICR_OFF;
        _rdr = binding.UsartBase + Stm32l476UsartLayout.RDR_OFF;
        _tdr = binding.UsartBase + Stm32l476UsartLayout.TDR_OFF;
    }

    /// <summary>Brings the bound USART up at the binding's rate, 8N1: gates the pin port's clock
    /// and the USART's, muxes the TX/RX pair to its alternate function, programs the divisor, then
    /// enables the USART with transmitter and receiver. Idempotent -- safe over a USART the
    /// resident firmware already configured.</summary>
    public void Init()
    {
        Mmio.Write32(_binding.PortRccEnReg,
            Mmio.Read32(_binding.PortRccEnReg) | _binding.PortRccEnMask);
        Mmio.Write32(_binding.RccEnReg, Mmio.Read32(_binding.RccEnReg) | _binding.RccEnMask);

        Mmio.Write32(_cr1, 0u);

        Mmio.Write32(_binding.ModerReg,
            (Mmio.Read32(_binding.ModerReg) & ~_binding.ModerMask) | _binding.ModerValue);
        Mmio.Write32(_binding.AfrReg,
            (Mmio.Read32(_binding.AfrReg) & ~_binding.AfrMask) | _binding.AfrValue);

        Mmio.Write32(_brr, _binding.BaudDivisor);
        Mmio.Write32(_cr1, Stm32l476UsartLayout.CR1_UE | Stm32l476UsartLayout.CR1_TE
            | Stm32l476UsartLayout.CR1_RE);
    }

    /// <summary>Sends one byte, waiting (bounded) for the transmit register to empty first.</summary>
    public void WriteByte(int value)
    {
        for (int spin = 0; spin < 1000000; spin++)
        {
            if ((Mmio.Read32(_isr) & Stm32l476UsartLayout.ISR_TXE) != 0u) break;
        }
        Mmio.Write32(_tdr, (uint)(value & 0xFF));
    }

    /// <summary>Sends a string as its low-byte (ASCII) characters.</summary>
    public void Write(string text)
    {
        for (int i = 0; i < text.Length; i++)
        {
            WriteByte(text[i]);
        }
    }

    /// <summary>Waits (bounded) until the last frame has left the shift register (ISR.TC), so a
    /// mode switch or a caller's delay never cuts a frame on the wire.</summary>
    public void Flush()
    {
        for (int spin = 0; spin < 1000000; spin++)
        {
            if ((Mmio.Read32(_isr) & Stm32l476UsartLayout.ISR_TC) != 0u) return;
        }
    }

    /// <summary>1 when at least one received byte waits (ISR.RXNE), else 0.</summary>
    public int Available
    {
        get
        {
            return (Mmio.Read32(_isr) & Stm32l476UsartLayout.ISR_RXNE) != 0u ? 1 : 0;
        }
    }

    /// <summary>Pops one received byte (the RDR field is 9 bits wide), or -1 when the receive
    /// register is empty (the Stream convention).</summary>
    public int ReadByte()
    {
        uint status = Mmio.Read32(_isr);

        if ((status & Stm32l476UsartLayout.ISR_ORE) != 0u)
        {
            Mmio.Write32(_icr, Stm32l476UsartLayout.ICR_ORECF);
        }

        if ((status & Stm32l476UsartLayout.ISR_RXNE) == 0u) return -1;
        return (int)(Mmio.Read32(_rdr) & Stm32l476UsartLayout.RDR_RDR);
    }
}
