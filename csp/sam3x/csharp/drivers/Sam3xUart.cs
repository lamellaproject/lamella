// The SAM3X UART driver, in C# over Lamella.Hardware.Mmio -- ONE driver for every UART binding on
using Lamella.Boards;
using Lamella.Generated;
using Lamella.Hardware;

public sealed class Sam3xUart
{
    private readonly uint _cr;
    private readonly uint _mr;
    private readonly uint _idr;
    private readonly uint _sr;
    private readonly uint _rhr;
    private readonly uint _thr;
    private readonly uint _brgr;
    private readonly Sam3xUartBinding _binding;

    /// <summary>Binds the driver to one UART wiring; no hardware is touched until
    /// <see cref="Init"/>.</summary>
    public Sam3xUart(Sam3xUartBinding binding)
    {
        _binding = binding;
        _cr = binding.UartBase + Sam3xUartLayout.CR_OFF;
        _mr = binding.UartBase + Sam3xUartLayout.MR_OFF;
        _idr = binding.UartBase + Sam3xUartLayout.IDR_OFF;
        _sr = binding.UartBase + Sam3xUartLayout.SR_OFF;
        _rhr = binding.UartBase + Sam3xUartLayout.RHR_OFF;
        _thr = binding.UartBase + Sam3xUartLayout.THR_OFF;
        _brgr = binding.UartBase + Sam3xUartLayout.BRGR_OFF;
    }

    /// <summary>Brings the bound UART up at the binding's rate, 8N1: gates the peripheral clock,
    /// hands the TX/RX lines to their PIO peripheral function, resets the block, then configures
    /// and enables it. Assumes the board's default clock plan is already running -- the binding's
    /// divisor is only correct under that MCK, which the board's firmware establishes at boot.
    /// Idempotent -- safe over a UART the resident firmware already configured.</summary>
    public void Init()
    {
        Mmio.Write32(_binding.PmcPcerReg, _binding.PmcPcerMask);

        if (_binding.PioFunc != 0u)
        {
            Mmio.Write32(_binding.PioAbsrReg, Mmio.Read32(_binding.PioAbsrReg) | _binding.PioMask);
        }
        Mmio.Write32(_binding.PioPdrReg, _binding.PioMask);

        Mmio.Write32(_cr, Sam3xUartLayout.CR_RSTRX | Sam3xUartLayout.CR_RSTTX
            | Sam3xUartLayout.CR_RSTSTA);
        Mmio.Write32(_idr, Sam3xUartLayout.IDR_RXRDY | Sam3xUartLayout.IDR_TXRDY
            | Sam3xUartLayout.IDR_OVRE | Sam3xUartLayout.IDR_FRAME | Sam3xUartLayout.IDR_PARE
            | Sam3xUartLayout.IDR_TXEMPTY);
        Mmio.Write32(_mr, Sam3xUartLayout.MR_8N1);
        Mmio.Write32(_brgr, _binding.BaudDivisor);
        Mmio.Write32(_cr, Sam3xUartLayout.CR_RXEN | Sam3xUartLayout.CR_TXEN);
    }

    /// <summary>Sends one byte, waiting (bounded) for the transmit holding register to be ready
    /// first.</summary>
    public void WriteByte(int value)
    {
        for (int spin = 0; spin < 1000000; spin++)
        {
            if ((Mmio.Read32(_sr) & Sam3xUartLayout.SR_TXRDY) != 0u) break;
        }
        Mmio.Write32(_thr, (uint)(value & 0xFF));
    }

    /// <summary>Sends a string as its low-byte (ASCII) characters.</summary>
    public void Write(string text)
    {
        for (int i = 0; i < text.Length; i++)
        {
            WriteByte(text[i]);
        }
    }

    /// <summary>Waits (bounded) until the last frame has left the shift register (SR.TXEMPTY), so
    /// a caller's reset or delay never cuts a frame on the wire.</summary>
    public void Flush()
    {
        for (int spin = 0; spin < 1000000; spin++)
        {
            if ((Mmio.Read32(_sr) & Sam3xUartLayout.SR_TXEMPTY) != 0u) return;
        }
    }

    /// <summary>1 when a received byte waits (SR.RXRDY), else 0.</summary>
    public int Available
    {
        get
        {
            return (Mmio.Read32(_sr) & Sam3xUartLayout.SR_RXRDY) != 0u ? 1 : 0;
        }
    }

    /// <summary>Pops one received byte, or -1 when the receive register is empty (the Stream
    /// convention: absence is DATA, not an error).</summary>
    public int ReadByte()
    {
        if ((Mmio.Read32(_sr) & Sam3xUartLayout.SR_RXRDY) == 0u) return -1;
        return (int)(Mmio.Read32(_rhr) & Sam3xUartLayout.RHR_RXCHR);
    }
}
