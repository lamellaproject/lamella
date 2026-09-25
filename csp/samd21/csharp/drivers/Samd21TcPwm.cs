#if LAMELLA_SURFACE_FLOAT
// A SAMD21 TC as a PWM counter, in its 8-bit mode: single-slope PWM with PER as TOP, so both of
// its waveform outputs carry a duty cycle of their own (DS40001882D 30.6.2.6).
//
// Eight bits bound the period at 255 prescaled clocks. From an 8 MHz clock that is 31 Hz to 4 MHz,
// and a 50 Hz servo gets 156 duty-cycle steps; a TCC's longer counter gives a finer one.
//
// The TC buffers neither its period nor its compare values, so a change while it runs takes effect
// at once. A TOP written below the count lets the counter run on to 255 and wrap before it
// matches, stretching that one period (30.6.2.6.4), and until the period ends a new compare value
// meets the old TOP.
using Lamella.Boards;
using Lamella.Generated;
using Lamella.Hardware;

public sealed class Samd21TcPwm : Samd21PwmCounter
{
    readonly uint _ctrla;
    readonly uint _status;
    readonly uint _count;
    readonly uint _per;
    readonly uint _cc0;

    /// <summary>Binds the driver to a TC and the outputs the board wired to it. No hardware is
    /// touched until a channel is created.</summary>
    public Samd21TcPwm(Samd21PwmBinding binding) : base(binding)
    {
        uint block = binding.CounterBase;
        _ctrla = block + Samd21TcCount8Layout.CTRLA_OFF;
        _status = block + Samd21TcCount8Layout.STATUS_OFF;
        _count = block + Samd21TcCount8Layout.COUNT_OFF;
        _per = block + Samd21TcCount8Layout.PER_OFF;
        _cc0 = block + Samd21TcCount8Layout.CC0_OFF;
    }

    protected override uint Divisor(uint code)
    {
        return Samd21TcCount8Layout.PrescalerDivisor(code);
    }

    protected override bool Reset()
    {
        // Disabled first, as the reset requires (30.6.2.2). The reset spares DBGCTRL.
        uint ctrla = Mmio.Read16(_ctrla);
        if ((ctrla & Samd21TcCount8Layout.CTRLA_ENABLE) != 0u)
        {
            Mmio.Write16(_ctrla, (ushort)(ctrla & ~Samd21TcCount8Layout.CTRLA_ENABLE));
            if (!WaitSync())
            {
                return false;
            }
        }
        Mmio.Write16(_ctrla, (ushort)Samd21TcCount8Layout.CTRLA_SWRST);
        return WaitReset();
    }

    protected override bool Configure(uint prescaler, uint top)
    {
        // The mode first: PER and the 8-bit compare registers exist only in the 8-bit mode. MODE,
        // WAVEGEN and PRESCALER are enable-protected, and the counter is disabled here.
        Mmio.Write16(_ctrla, (ushort)((Samd21TcCount8Layout.MODE_COUNT8 << (int)Samd21TcCount8Layout.CTRLA_MODE_LSB)
            | (Samd21TcCount8Layout.WAVEGEN_NPWM << (int)Samd21TcCount8Layout.CTRLA_WAVEGEN_LSB)
            | (prescaler << (int)Samd21TcCount8Layout.CTRLA_PRESCALER_LSB)));
        if (!WaitSync())
        {
            return false;
        }
        Mmio.Write8(_count, (byte)0);
        if (!WaitSync())
        {
            return false;
        }
        Mmio.Write8(_per, (byte)top);
        return WaitSync();
    }

    protected override bool ChangeTop(uint top)
    {
        Mmio.Write8(_per, (byte)top);
        return WaitSync();
    }

    protected override bool WriteCompare(int compareChannel, uint value, bool running)
    {
        // Running or not, a compare register takes its value at once.
        Mmio.Write8(_cc0 + (uint)compareChannel * Samd21TcCount8Layout.CC_STRIDE, (byte)value);
        return WaitSync();
    }

    protected override bool SetEnabled(bool enable)
    {
        uint ctrla = Mmio.Read16(_ctrla);
        if (enable)
        {
            ctrla = ctrla | Samd21TcCount8Layout.CTRLA_ENABLE;
        }
        else
        {
            ctrla = ctrla & ~Samd21TcCount8Layout.CTRLA_ENABLE;
        }
        Mmio.Write16(_ctrla, (ushort)ctrla);
        return WaitSync();
    }

    // The registers written with synchronization (30.6.6) finish when STATUS.SYNCBUSY clears, and a
    // second such write before then would stall the bus until it did.
    bool WaitSync()
    {
        for (int spin = 0; spin < WaitBound; spin++)
        {
            if ((Mmio.Read8(_status) & Samd21TcCount8Layout.STATUS_SYNCBUSY) == 0u)
            {
                return true;
            }
        }
        return false;
    }

    // A software reset is complete when CTRLA.SWRST and STATUS.SYNCBUSY have both cleared (30.8.1).
    bool WaitReset()
    {
        for (int spin = 0; spin < WaitBound; spin++)
        {
            if ((Mmio.Read16(_ctrla) & Samd21TcCount8Layout.CTRLA_SWRST) == 0u
                && (Mmio.Read8(_status) & Samd21TcCount8Layout.STATUS_SYNCBUSY) == 0u)
            {
                return true;
            }
        }
        return false;
    }
}
#endif
