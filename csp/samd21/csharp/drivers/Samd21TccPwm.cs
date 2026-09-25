#if LAMELLA_SURFACE_FLOAT
// A SAMD21 TCC as a PWM counter: single-slope PWM with PER as TOP, each compare channel the duty
// cycle of the waveform output it drives (DS40001882D 31.6.2.5.5).
//
// Sixteen bits on TCC2 bound the period at 65,535 prescaled clocks. From an 8 MHz clock that is
// 1 Hz to 4 MHz, and a 50 Hz servo gets 40,000 duty-cycle steps.
//
// A running change goes through the buffer registers, PERB and CCBx, which the counter copies into
// PER and CCx when it wraps (31.6.2.6), so a period never mixes an old value with a new one. A new
// period and the compare values that go with it are written with updates locked, so they take
// effect at the same wrap. While the counter is disabled, each value is written to its register and
// to its buffer both, so a buffered value left from before the counter stopped cannot replace it
// at the first wrap after the counter starts again.
using Lamella.Boards;
using Lamella.Generated;
using Lamella.Hardware;

public sealed class Samd21TccPwm : Samd21PwmCounter
{
    readonly uint _ctrla;
    readonly uint _ctrlbclr;
    readonly uint _ctrlbset;
    readonly uint _syncbusy;
    readonly uint _count;
    readonly uint _wave;
    readonly uint _per;
    readonly uint _perb;
    readonly uint _cc0;
    readonly uint _ccb0;

    /// <summary>Binds the driver to a TCC and the outputs the board wired to it. No hardware is
    /// touched until a channel is created.</summary>
    public Samd21TccPwm(Samd21PwmBinding binding) : base(binding)
    {
        uint block = binding.CounterBase;
        _ctrla = block + Samd21TccLayout.CTRLA_OFF;
        _ctrlbclr = block + Samd21TccLayout.CTRLBCLR_OFF;
        _ctrlbset = block + Samd21TccLayout.CTRLBSET_OFF;
        _syncbusy = block + Samd21TccLayout.SYNCBUSY_OFF;
        _count = block + Samd21TccLayout.COUNT_OFF;
        _wave = block + Samd21TccLayout.WAVE_OFF;
        _per = block + Samd21TccLayout.PER_OFF;
        _perb = block + Samd21TccLayout.PERB_OFF;
        _cc0 = block + Samd21TccLayout.CC0_OFF;
        _ccb0 = block + Samd21TccLayout.CCB0_OFF;
    }

    protected override uint Divisor(uint code)
    {
        return Samd21TccLayout.PrescalerDivisor(code);
    }

    protected override bool Reset()
    {
        // Disabled first, as the reset requires (31.6.2.2). The reset spares DBGCTRL.
        uint ctrla = Mmio.Read32(_ctrla);
        if ((ctrla & Samd21TccLayout.CTRLA_ENABLE) != 0u)
        {
            Mmio.Write32(_ctrla, ctrla & ~Samd21TccLayout.CTRLA_ENABLE);
            if (!WaitSync(Samd21TccLayout.SYNCBUSY_ENABLE))
            {
                return false;
            }
        }
        Mmio.Write32(_ctrla, Samd21TccLayout.CTRLA_SWRST);
        if (!WaitReset())
        {
            return false;
        }
        Mmio.Write32(_wave, Samd21TccLayout.WAVEGEN_NPWM << (int)Samd21TccLayout.WAVE_WAVEGEN_LSB);
        return WaitSync(Samd21TccLayout.SYNCBUSY_WAVE);
    }

    protected override bool Configure(uint prescaler, uint top)
    {
        // PRESCALER is enable-protected, and the counter is disabled here. Only ENABLE and SWRST of
        // CTRLA are synchronized, so this write has nothing to wait for.
        Mmio.Write32(_ctrla, prescaler << (int)Samd21TccLayout.CTRLA_PRESCALER_LSB);
        Mmio.Write32(_count, 0u);
        if (!WaitSync(Samd21TccLayout.SYNCBUSY_COUNT))
        {
            return false;
        }
        Mmio.Write32(_per, top);
        if (!WaitSync(Samd21TccLayout.SYNCBUSY_PER))
        {
            return false;
        }
        Mmio.Write32(_perb, top);
        return WaitSync(Samd21TccLayout.SYNCBUSY_PERB);
    }

    protected override bool ChangeTop(uint top)
    {
        Mmio.Write32(_perb, top);
        return WaitSync(Samd21TccLayout.SYNCBUSY_PERB);
    }

    protected override bool WriteCompare(int compareChannel, uint value, bool running)
    {
        uint offset = (uint)compareChannel * Samd21TccLayout.CC_STRIDE;
        if (!running)
        {
            Mmio.Write32(_cc0 + offset, value);
            if (!WaitSync(Samd21TccLayout.SYNCBUSY_CC0 << compareChannel))
            {
                return false;
            }
        }
        Mmio.Write32(_ccb0 + offset, value);
        return WaitSync(Samd21TccLayout.SYNCBUSY_CCB0 << compareChannel);
    }

    protected override bool SetEnabled(bool enable)
    {
        uint ctrla = Mmio.Read32(_ctrla);
        if (enable)
        {
            ctrla = ctrla | Samd21TccLayout.CTRLA_ENABLE;
        }
        else
        {
            ctrla = ctrla & ~Samd21TccLayout.CTRLA_ENABLE;
        }
        Mmio.Write32(_ctrla, ctrla);
        return WaitSync(Samd21TccLayout.SYNCBUSY_ENABLE);
    }

    // CTRLB.LUPD set keeps the buffers from being copied at a wrap; cleared, the next wrap copies
    // every buffer written meanwhile (31.8.2 and 31.8.3).
    protected override bool HoldUpdates(bool hold)
    {
        Mmio.Write8(hold ? _ctrlbset : _ctrlbclr, (byte)Samd21TccLayout.CTRLBSET_LUPD);
        return WaitSync(Samd21TccLayout.SYNCBUSY_CTRLB);
    }

    // Each register written with synchronization has its own SYNCBUSY bit (31.8.4), and a write to a
    // register whose bit is set is discarded (DS40001882D 14.3.2.2), so every write waits for its bit.
    bool WaitSync(uint bit)
    {
        for (int spin = 0; spin < WaitBound; spin++)
        {
            if ((Mmio.Read32(_syncbusy) & bit) == 0u)
            {
                return true;
            }
        }
        return false;
    }

    // A software reset is complete when CTRLA.SWRST and SYNCBUSY.SWRST have both cleared (31.8.1).
    bool WaitReset()
    {
        for (int spin = 0; spin < WaitBound; spin++)
        {
            if ((Mmio.Read32(_ctrla) & Samd21TccLayout.CTRLA_SWRST) == 0u
                && (Mmio.Read32(_syncbusy) & Samd21TccLayout.SYNCBUSY_SWRST) == 0u)
            {
                return true;
            }
        }
        return false;
    }
}
#endif
