#if LAMELLA_SURFACE_FLOAT
// An RP2350 PWM slice driving pulse-width modulated outputs as dotnet/iot's PwmChannel: a board binds
// one slice per PWM chip, and each output it routed -- A as channel 0, B as channel 1 -- is one channel
// of that chip, created by PwmChannel.Create.
//
// ONE RATE PER SLICE. The slice's period is the period of both its outputs, and each output owns only
// its half of the compare register, which sets its duty cycle. So a frequency asked of one output
// re-times the slice and its other output if that is not started: it keeps its duty cycle, and its
// Frequency then reads the new rate. A started output is never re-timed under its owner, so a
// frequency other than the one a started output runs at is refused, naming the slice and that rate. An
// output may change its own frequency while it runs when it is the only one started.
//
// The period is TOP + 1 counts, and the slice counts at clk_sys divided by an integer from 1 to 255
// (datasheet 12.5.2.2, Table 1133). The block can divide by 256 too, but the rates only that divider
// reaches span a fraction of a hertz, and a frequency here is a whole number of hertz. The divider's
// fraction stays 0: it is a first-order sigma-delta, which moves the edges from one period to the
// next. TOP is at most 65534, so that a compare value of
// TOP + 1, a duty cycle of 1, still fits the compare register's 16 bits. The divider is the smallest
// whose period fits, which gives the most duty-cycle steps the rate allows, TOP + 1 of them. A rate
// between two periods takes the nearer one, and Frequency and DutyCycle read back what was asked, as
// dotnet/iot's own channels do.
//
// CC and TOP are double-buffered and take a written value when the counter wraps (12.5.2.3), so a new
// duty cycle, or a new period at the same divider, starts at a period boundary. The divider takes a
// write at once, so a period that needs a new divider stops the slice, sets it up and starts it again
// from a count of 0.
//
// Nothing is touched until a channel is created. The first creation releases the PWM block and both IO
// banks from reset and brings the slice up disabled. Starting an output hands its pad to the slice --
// the pad's function select first, then its isolation and output disable cleared, the order 9.7 gives
// -- and enables the slice. Stopping an output gives its pad back what it had, and stopping the last
// one disables the slice. Every wait is bounded, and one that runs out is reported as
// InvalidOperationException rather than waited on forever.
using System.Device.Pwm;
using Lamella.Boards;
using Lamella.Generated;
using Lamella.Hardware;

public sealed class Rp2350PwmSlice
{
    /// <summary>A bound on every hardware wait, so a block that never answers is reported rather than
    /// waited on forever.</summary>
    const int WaitBound = 100000;

    const uint TopMax = 0xFFFEu;

    readonly Rp2350PwmBinding _binding;
    readonly uint _csr;
    readonly uint _div;
    readonly uint _ctr;
    readonly uint _cc;
    readonly uint _top;
    readonly uint _resetsClr;
    readonly uint _resetsDone;

    // Per output, A then B: whether a channel holds it, whether it is started, the duty cycle asked of
    // it, and its pad's CTRL and PADS words as found at its start, which its stop puts back.
    readonly bool[] _open;
    readonly bool[] _started;
    readonly double[] _duty;
    readonly uint[] _ctrlFound;
    readonly uint[] _padFound;

    // The slice: the rate asked of it, the divider and TOP that produce that rate, whether it has been
    // brought up, and whether it is enabled.
    int _rate;
    uint _divider;
    uint _period;
    bool _up;
    bool _enabled;

    /// <summary>Binds the slice and the outputs the board routed from it. No hardware is touched until
    /// a channel is created.</summary>
    public Rp2350PwmSlice(Rp2350PwmBinding binding)
    {
        _binding = binding;
        uint slice = binding.PwmBase + Rp2350PwmLayout.CH_STRIDE * binding.Slice;
        _csr = slice + Rp2350PwmLayout.CH0_CSR_OFF;
        _div = slice + Rp2350PwmLayout.CH0_DIV_OFF;
        _ctr = slice + Rp2350PwmLayout.CH0_CTR_OFF;
        _cc = slice + Rp2350PwmLayout.CH0_CC_OFF;
        _top = slice + Rp2350PwmLayout.CH0_TOP_OFF;
        _resetsClr = Rp2350Instances.RESETS_CLR_BASE + Rp2350ResetsLayout.RESET_OFF;
        _resetsDone = Rp2350Instances.RESETS_BASE + Rp2350ResetsLayout.RESET_DONE_OFF;
        _open = new bool[2];
        _started = new bool[2];
        _duty = new double[2];
        _ctrlFound = new uint[2];
        _padFound = new uint[2];
    }

    /// <summary>Creates output <paramref name="channel"/> of this slice, 0 for A and 1 for B, as a
    /// channel set to <paramref name="frequency"/> and <paramref name="dutyCyclePercentage"/> and not
    /// yet started: the body of a board's <c>PwmChannelFactory</c>. The channel is the caller's, and
    /// disposing it releases the output, so it can be created again.</summary>
    /// <exception cref="System.ArgumentOutOfRangeException">The board routed no such output, the
    /// frequency is outside what the slice can produce, or the duty cycle is outside 0.0 to
    /// 1.0.</exception>
    /// <exception cref="System.InvalidOperationException">The output is already open, the slice's other
    /// output is started at a different frequency, or the block did not come out of reset.</exception>
    public PwmChannel Open(int channel, int frequency, double dutyCyclePercentage)
    {
        if ((object)_binding.Output(channel) == null)
        {
            throw new System.ArgumentOutOfRangeException("channel");
        }
        if (_open[channel])
        {
            throw new System.InvalidOperationException("PWM channel " + channel
                + " is already open; dispose it before creating it again");
        }
        uint divider;
        uint top;
        if (!Period(frequency, out divider, out top))
        {
            throw new System.ArgumentOutOfRangeException("frequency");
        }
        CheckDuty(dutyCyclePercentage, "dutyCyclePercentage");
        KeepRate(channel, frequency);
        if (!_up)
        {
            BringUp();
        }
        _duty[channel] = dutyCyclePercentage;
        _open[channel] = true;
        if (!Retime(frequency, divider, top))
        {
            WriteCompares();
        }
        return new Rp2350PwmChannel(this, channel);
    }

    // The rate both outputs of the slice read as their Frequency.
    internal int Rate { get { return _rate; } }

    internal double DutyOf(int output)
    {
        return _duty[output];
    }

    internal void SetFrequency(int output, int frequency)
    {
        uint divider;
        uint top;
        if (!Period(frequency, out divider, out top))
        {
            throw new System.ArgumentOutOfRangeException("value");
        }
        KeepRate(output, frequency);
        Retime(frequency, divider, top);
    }

    internal void SetDutyCycle(int output, double dutyCycle)
    {
        CheckDuty(dutyCycle, "value");
        _duty[output] = dutyCycle;
        WriteCompares();
    }

    // Hands the output's pad to the slice and enables the slice if it is not running.
    internal void Start(int output)
    {
        if (_started[output])
        {
            return;
        }
        Rp2350PwmOutput pad = _binding.Output(output);
        _ctrlFound[output] = Mmio.Read32(pad.IoCtrl);
        _padFound[output] = Mmio.Read32(pad.Pads);
        // The function first, then the pad out of isolation, so the pad never drives another
        // function's level (datasheet 9.7). The slice drives the pad, which needs no input buffer.
        Mmio.Write32(pad.IoCtrl, _binding.Funcsel);
        Mmio.Write32(pad.Pads, _padFound[output] & ~(Rp2350PadsBank0Layout.GPIO0_ISO | Rp2350PadsBank0Layout.GPIO0_OD));
        _started[output] = true;
        if (!_enabled)
        {
            Enable(true);
        }
    }

    // Gives the output's pad back the function and pad setting it had at the start, and disables the
    // slice when neither of its outputs is started.
    internal void Stop(int output)
    {
        if (!_started[output])
        {
            return;
        }
        Rp2350PwmOutput pad = _binding.Output(output);
        // The function first, so a pad that was isolated at the start is isolated again with the
        // slice no longer driving it.
        Mmio.Write32(pad.IoCtrl, _ctrlFound[output]);
        Mmio.Write32(pad.Pads, _padFound[output]);
        _started[output] = false;
        if (!_started[0] && !_started[1])
        {
            Enable(false);
        }
    }

    // Stops the output and gives it up, so it can be created again.
    internal void Release(int output)
    {
        Stop(output);
        _open[output] = false;
    }

    // Refuses, before anything changes, a frequency other than the one the slice's other output runs at
    // while it is started.
    void KeepRate(int requester, int frequency)
    {
        int other = 1 - requester;
        if (_started[other] && frequency != _rate)
        {
            throw new System.InvalidOperationException("PWM channel " + other
                + " is started and runs this chip's slice, slice " + _binding.Slice + ", at " + _rate
                + " Hz, which a started channel keeps: stop channel " + other + " or ask for " + _rate + " Hz");
        }
    }

    // Times the brought-up slice for frequency. An open output keeps its duty cycle over the new period.
    // False when the period is the one the slice already has, so nothing was written.
    bool Retime(int frequency, uint divider, uint top)
    {
        if (divider == _divider && top == _period)
        {
            // The same period, so only the rate the outputs read back changes.
            _rate = frequency;
            return false;
        }
        // The divider takes a write at once, so a running slice stops while it changes. By KeepRate
        // only the requester can be running then, so only its output is interrupted.
        bool restart = _enabled && divider != _divider;
        if (restart)
        {
            Enable(false);
        }
        _period = top;
        _rate = frequency;
        if (_enabled)
        {
            // CC and then TOP, each taken at the next wrap. A wrap that falls between the two writes
            // gives one period the new compare values at the old TOP: nothing in the block lets the
            // two change together.
            WriteCompares();
            Mmio.Write32(_top, top);
        }
        else
        {
            // Disabled, the slice takes every write at once; the count restarts at 0, so the first
            // period after the slice is enabled is a whole one.
            Mmio.Write32(_div, DividerWord(divider));
            Mmio.Write32(_top, top);
            Mmio.Write32(_ctr, 0u);
            WriteCompares();
        }
        _divider = divider;
        if (restart)
        {
            Enable(true);
        }
        return true;
    }

    // The PWM block and both IO banks out of reset, then the slice disabled, counting freely at the
    // divided rate, trailing-edge, neither output inverted.
    void BringUp()
    {
        Mmio.Write32(_resetsClr, _binding.ResetMask);
        bool released = false;
        for (int spin = 0; spin < WaitBound; spin++)
        {
            if ((Mmio.Read32(_resetsDone) & _binding.ResetMask) == _binding.ResetMask)
            {
                released = true;
                break;
            }
        }
        if (!released)
        {
            throw new System.InvalidOperationException("the PWM block did not come out of reset");
        }
        Mmio.Write32(_csr, Rp2350PwmLayout.DIVMODE_DIV << (int)Rp2350PwmLayout.CH0_CSR_DIVMODE_LSB);
        _up = true;
    }

    void Enable(bool enable)
    {
        uint csr = Rp2350PwmLayout.DIVMODE_DIV << (int)Rp2350PwmLayout.CH0_CSR_DIVMODE_LSB;
        if (enable)
        {
            csr |= Rp2350PwmLayout.CH0_CSR_EN;
        }
        Mmio.Write32(_csr, csr);
        _enabled = enable;
    }

    // Both outputs' compare values for their duty cycles over the current period, in one write: A in
    // the register's low half and B in its high half. An output that is not open compares at 0, a duty
    // cycle of 0.
    void WriteCompares()
    {
        uint cc = 0u;
        for (int output = 0; output < 2; output++)
        {
            if (!_open[output])
            {
                continue;
            }
            uint compare = (uint)(_duty[output] * (double)(_period + 1u) + 0.5);
            cc |= compare << (output == 0 ? (int)Rp2350PwmLayout.CH0_CC_A_LSB : (int)Rp2350PwmLayout.CH0_CC_B_LSB);
        }
        Mmio.Write32(_cc, cc);
    }

    // DIV for a whole divider from 1 to 255: INT holds it, and FRAC stays 0.
    static uint DividerWord(uint divider)
    {
        return divider << (int)Rp2350PwmLayout.CH0_DIV_INT_LSB;
    }

    // The divider and TOP nearest to frequency, with the smallest divider whose period fits. False when
    // none does: the frequency is below what the largest divider reaches, or above half of clk_sys,
    // where a period is too short to hold a duty cycle.
    bool Period(int frequency, out uint divider, out uint top)
    {
        divider = 0u;
        top = 0u;
        uint clock = _binding.ClkSysHz;
        if (frequency < 1 || (uint)frequency > clock / 2u)
        {
            return false;
        }
        uint rate = (uint)frequency;
        for (uint candidate = 1u; candidate <= 255u; candidate++)
        {
            // Not above clock / candidate, which also keeps candidate * rate within 32 bits.
            if (rate > clock / candidate)
            {
                break;
            }
            uint step = candidate * rate;
            uint ticks = (clock + step / 2u) / step;
            if (ticks < 2u || ticks - 1u > TopMax)
            {
                continue;
            }
            divider = candidate;
            top = ticks - 1u;
            return true;
        }
        return false;
    }

    static void CheckDuty(double dutyCycle, string name)
    {
        // Written so that a NaN fails it too.
        if (!(dutyCycle >= 0.0 && dutyCycle <= 1.0))
        {
            throw new System.ArgumentOutOfRangeException(name);
        }
    }
}

// One output of an Rp2350PwmSlice, as the PwmChannel its owner holds.
internal sealed class Rp2350PwmChannel : PwmChannel
{
    readonly Rp2350PwmSlice _slice;
    readonly int _output;
    bool _disposed;

    internal Rp2350PwmChannel(Rp2350PwmSlice slice, int output)
    {
        _slice = slice;
        _output = output;
    }

    /// <summary>The slice's rate in hertz, which both its outputs share: the rate this channel asked
    /// for last, or the one the other output set since while this one was stopped.</summary>
    /// <exception cref="System.ArgumentOutOfRangeException">The frequency is outside what the slice can
    /// produce.</exception>
    /// <exception cref="System.InvalidOperationException">The slice's other output is started at a
    /// different frequency.</exception>
    public override int Frequency
    {
        get { return _slice.Rate; }
        set
        {
            CheckLive();
            _slice.SetFrequency(_output, value);
        }
    }

    /// <summary>The duty cycle this channel asked for, from 0.0 to 1.0.</summary>
    /// <exception cref="System.ArgumentOutOfRangeException">The duty cycle is outside 0.0 to
    /// 1.0.</exception>
    public override double DutyCycle
    {
        get { return _slice.DutyOf(_output); }
        set
        {
            CheckLive();
            _slice.SetDutyCycle(_output, value);
        }
    }

    public override void Start()
    {
        CheckLive();
        _slice.Start(_output);
    }

    public override void Stop()
    {
        if (!_disposed)
        {
            _slice.Stop(_output);
        }
    }

    protected override void Dispose(bool disposing)
    {
        if (!_disposed)
        {
            _slice.Release(_output);
            _disposed = true;
        }
    }

    void CheckLive()
    {
        if (_disposed)
        {
            throw new System.ObjectDisposedException("PwmChannel", "the PWM channel is disposed; create it again");
        }
    }
}
#endif
