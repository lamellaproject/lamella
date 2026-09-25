#if LAMELLA_SURFACE_FLOAT
// A SAMD21 timer/counter driving pulse-width modulated outputs as dotnet/iot's PwmChannel: a board
// binds one counter per PWM chip, and each output it wired to the counter is one channel of that
// chip, created by PwmChannel.Create. Samd21TcPwm and Samd21TccPwm are its two register backends.
//
// ONE RATE PER COUNTER. The counter's period is the period of every output it drives, and each
// output owns only its compare value, which sets its duty cycle. So a frequency asked of one
// output re-times the counter and every output of it that is not started: each keeps its duty
// cycle, and its Frequency then reads the new rate. A started output is never re-timed under its
// owner, so a frequency other than the one a started output runs at is refused, naming the counter
// and that rate. An output may change its own frequency while it runs when it is the only one
// started.
//
// The period is TOP + 1 prescaled clocks. TOP is at most 2^n - 2 for an n-bit counter, so that a
// compare value of TOP + 1 still fits: it never matches, which holds the output set for a duty
// cycle of 1. The prescaler is the smallest division whose period fits, which gives the most
// duty-cycle steps the rate allows, TOP + 1 of them. A rate between two periods takes the nearer
// one, and Frequency and DutyCycle read back what was asked, as dotnet/iot's own channels do.
//
// Nothing is touched until a channel is created. The first creation brings the counter up
// disabled: its bus clock, its generic clock routed as DS40001882D 15.6.3.3 requires, a reset, the
// waveform and the period. Starting an output hands its pad to the counter and enables the
// counter; stopping the last started output disables it. Every wait is bounded, and one that runs
// out is reported as InvalidOperationException rather than waited on forever.
//
// The counter halts while a debugger halts the core unless its DBGCTRL.DBGRUN is set, and this
// driver leaves DBGCTRL as it finds it. Without LAMELLA_SURFACE_FLOAT there is no duty cycle to
// set, so the counter and both backends are compiled only with it.
using System.Device.Pwm;
using Lamella.Boards;
using Lamella.Generated;
using Lamella.Hardware;

public abstract class Samd21PwmCounter
{
    /// <summary>A bound on every hardware wait, so a counter that never answers is reported rather
    /// than waited on forever.</summary>
    protected const int WaitBound = 100000;

    readonly Samd21PwmBinding _binding;
    readonly uint _apbcmask;
    readonly uint _topMax;

    // Per wired output: whether a channel holds it, whether it is started, the duty cycle asked of
    // it, and its pad's PMUX nibble and PINCFG byte as found at its start, which its stop puts back.
    readonly bool[] _open;
    readonly bool[] _started;
    readonly double[] _duty;
    readonly uint[] _pmuxFound;
    readonly uint[] _pincfgFound;

    // The counter: the rate asked of it, the PRESCALER code and TOP that produce that rate, whether
    // it has been brought up, and whether it is enabled.
    int _rate;
    uint _prescaler;
    uint _top;
    bool _up;
    bool _enabled;

    /// <summary>Binds the counter and the outputs the board wired to it. No hardware is touched
    /// until a channel is created.</summary>
    protected Samd21PwmCounter(Samd21PwmBinding binding)
    {
        _binding = binding;
        _apbcmask = Samd21Instances.PM_BASE + Samd21PmLayout.APBCMASK_OFF;
        _topMax = (1u << (int)binding.CounterBits) - 2u;
        int outputs = binding.OutputCount;
        _open = new bool[outputs];
        _started = new bool[outputs];
        _duty = new double[outputs];
        _pmuxFound = new uint[outputs];
        _pincfgFound = new uint[outputs];
    }

    /// <summary>Creates output <paramref name="channel"/> of this counter as a channel set to
    /// <paramref name="frequency"/> and <paramref name="dutyCyclePercentage"/> and not yet
    /// started: the body of a board's <c>PwmChannelFactory</c>. The channel is the caller's, and
    /// disposing it releases the output, so it can be created again.</summary>
    /// <exception cref="System.ArgumentOutOfRangeException">The board wired no such output, the
    /// frequency is outside what the counter can produce, or the duty cycle is outside 0.0 to
    /// 1.0.</exception>
    /// <exception cref="System.InvalidOperationException">The output is already open, another output
    /// of the counter is started at a different frequency, or the counter did not come
    /// up.</exception>
    public PwmChannel Open(int channel, int frequency, double dutyCyclePercentage)
    {
        if (channel < 0 || channel >= _open.Length)
        {
            throw new System.ArgumentOutOfRangeException("channel");
        }
        if (_open[channel])
        {
            throw new System.InvalidOperationException("PWM channel " + channel
                + " is already open; dispose it before creating it again");
        }
        uint prescaler;
        uint top;
        if (!Period(frequency, out prescaler, out top))
        {
            throw new System.ArgumentOutOfRangeException("frequency");
        }
        CheckDuty(dutyCyclePercentage, "dutyCyclePercentage");
        Retime(channel, frequency, prescaler, top);
        _duty[channel] = dutyCyclePercentage;
        WriteCompareOf(channel, _enabled);
        _open[channel] = true;
        return new Samd21PwmChannel(this, channel);
    }

    // The rate every output of the counter reads as its Frequency.
    internal int Rate { get { return _rate; } }

    internal double DutyOf(int output)
    {
        return _duty[output];
    }

    internal void SetFrequency(int output, int frequency)
    {
        uint prescaler;
        uint top;
        if (!Period(frequency, out prescaler, out top))
        {
            throw new System.ArgumentOutOfRangeException("value");
        }
        Retime(output, frequency, prescaler, top);
    }

    internal void SetDutyCycle(int output, double dutyCycle)
    {
        CheckDuty(dutyCycle, "value");
        _duty[output] = dutyCycle;
        WriteCompareOf(output, _enabled);
    }

    // Hands the output's pad to the counter and enables the counter if it is not running.
    internal void Start(int output)
    {
        if (_started[output])
        {
            return;
        }
        Samd21PwmOutput pad = _binding.Output(output);
        uint mask = Samd21PortLayout.PMUX0_PMUXE << (int)pad.PmuxShift;
        uint pmux = Mmio.Read8(pad.PmuxReg);
        _pmuxFound[output] = pmux & mask;
        _pincfgFound[output] = Mmio.Read8(pad.PincfgReg);
        // The function first and PMUXEN second, so the pad never runs another peripheral's function
        // between the two writes. PMUXEN alone: the counter drives the pad, which needs neither an
        // input buffer nor a pull.
        Mmio.Write8(pad.PmuxReg, (byte)((pmux & ~mask) | (_binding.PmuxFunc << (int)pad.PmuxShift)));
        Mmio.Write8(pad.PincfgReg, (byte)Samd21PortLayout.PINCFG0_PMUXEN);
        _started[output] = true;
        if (!_enabled)
        {
            Enable(true);
        }
    }

    // Gives the output's pad back the multiplexer setting and pin configuration it had at the start,
    // and disables the counter when no output of it is started.
    internal void Stop(int output)
    {
        if (!_started[output])
        {
            return;
        }
        Samd21PwmOutput pad = _binding.Output(output);
        uint mask = Samd21PortLayout.PMUX0_PMUXE << (int)pad.PmuxShift;
        // PINCFG first: a pad the PORT owned before the start goes straight back to the PORT, and
        // the nibble written after it is not seen.
        Mmio.Write8(pad.PincfgReg, (byte)_pincfgFound[output]);
        uint pmux = Mmio.Read8(pad.PmuxReg);
        Mmio.Write8(pad.PmuxReg, (byte)((pmux & ~mask) | _pmuxFound[output]));
        _started[output] = false;
        for (int i = 0; i < _started.Length; i++)
        {
            if (_started[i])
            {
                return;
            }
        }
        Enable(false);
    }

    // Stops the output and gives it up, so it can be created again.
    internal void Release(int output)
    {
        Stop(output);
        _open[output] = false;
    }

    /// <summary>The division PRESCALER code <paramref name="code"/> selects, or 0 for a code the
    /// counter does not have.</summary>
    protected abstract uint Divisor(uint code);

    /// <summary>Resets the counter, disabling it first as its reset requires, and selects
    /// single-slope PWM. False when a wait ran out.</summary>
    protected abstract bool Reset();

    /// <summary>Sets the disabled counter's prescaler and TOP, and restarts its count at zero, so the
    /// first period after it is enabled is a whole one. False when a wait ran out.</summary>
    protected abstract bool Configure(uint prescaler, uint top);

    /// <summary>Sets TOP while the counter runs, its prescaler unchanged. False when a wait ran
    /// out.</summary>
    protected abstract bool ChangeTop(uint top);

    /// <summary>Writes compare channel <paramref name="compareChannel"/>: at once while the counter
    /// is disabled, and as the counter takes a running change while <paramref name="running"/>.
    /// False when a wait ran out.</summary>
    protected abstract bool WriteCompare(int compareChannel, uint value, bool running);

    /// <summary>Enables or disables the counter, keeping the rest of its setup. False when a wait
    /// ran out.</summary>
    protected abstract bool SetEnabled(bool enable);

    /// <summary>While <paramref name="hold"/>, keeps a running counter's buffered writes from taking
    /// effect, so a period and its compare values change at the same update. A counter with no
    /// buffers has nothing to hold. False when a wait ran out.</summary>
    protected virtual bool HoldUpdates(bool hold)
    {
        return true;
    }

    // Times the counter for frequency on behalf of output requester. Every open output keeps its
    // duty cycle over the new period.
    void Retime(int requester, int frequency, uint prescaler, uint top)
    {
        for (int i = 0; i < _started.Length; i++)
        {
            if (i != requester && _started[i] && frequency != _rate)
            {
                throw new System.InvalidOperationException("PWM channel " + i
                    + " is started and runs this chip's counter, the one at 0x" + Hex(_binding.CounterBase)
                    + ", at " + _rate + " Hz, which a started channel keeps: stop channel " + i
                    + " or ask for " + _rate + " Hz");
            }
        }
        if (!_up)
        {
            BringUp();
        }
        else if (prescaler == _prescaler && top == _top)
        {
            // The same period, so only the rate the outputs read back changes.
            _rate = frequency;
            return;
        }
        // The prescaler is enable-protected, so a running counter stops while it changes. By the
        // check above only the requester can be running then, so only its output is interrupted.
        bool restart = _enabled && prescaler != _prescaler;
        if (restart)
        {
            Enable(false);
        }
        bool running = _enabled;
        if (running)
        {
            Check(HoldUpdates(true));
            Check(ChangeTop(top));
        }
        else
        {
            Check(Configure(prescaler, top));
        }
        _prescaler = prescaler;
        _top = top;
        _rate = frequency;
        for (int i = 0; i < _open.Length; i++)
        {
            if (_open[i])
            {
                WriteCompareOf(i, running);
            }
        }
        if (running)
        {
            Check(HoldUpdates(false));
        }
        if (restart)
        {
            Enable(true);
        }
    }

    // The bus clock, then the counter's own clock, then a reset. Without the bus clock the
    // counter's registers read as zero and ignore every write (DS40001882D 14.4).
    void BringUp()
    {
        Mmio.Write32(_apbcmask, Mmio.Read32(_apbcmask) | _binding.ApbcMask);
        if (!Samd21GenericClock.Route(_binding.GclkClkctrlValue))
        {
            throw new System.InvalidOperationException("the PWM counter's generic clock could not be routed"
                + " from its generator: it is locked to another generator, or it was running and did not stop");
        }
        Check(Reset());
        _up = true;
    }

    void Enable(bool enable)
    {
        Check(SetEnabled(enable));
        _enabled = enable;
    }

    // The output's compare value for its duty cycle over the current period.
    void WriteCompareOf(int output, bool running)
    {
        uint compare = (uint)(_duty[output] * (double)(_top + 1u) + 0.5);
        Check(WriteCompare(_binding.Output(output).CompareChannel, compare, running));
    }

    // The PRESCALER code and TOP nearest to frequency, with the smallest division whose period fits
    // the counter. False when none does: the frequency is below what the largest division reaches,
    // or above half the counter's clock, where a period is too short to hold a duty cycle.
    bool Period(int frequency, out uint prescaler, out uint top)
    {
        prescaler = 0u;
        top = 0u;
        uint clock = _binding.CoreClockHz;
        if (frequency < 1 || (uint)frequency > clock / 2u)
        {
            return false;
        }
        uint rate = (uint)frequency;
        uint chosen = 0u;
        for (uint code = 0u; ; code++)
        {
            uint division = Divisor(code);
            if (division == 0u)
            {
                break;
            }
            // Not above clock / division, which also keeps division * rate within 32 bits.
            if (rate > clock / division)
            {
                continue;
            }
            uint step = division * rate;
            uint ticks = (clock + step / 2u) / step;
            if (ticks < 2u || ticks - 1u > _topMax)
            {
                continue;
            }
            if (chosen == 0u || division < chosen)
            {
                chosen = division;
                prescaler = code;
                top = ticks - 1u;
            }
        }
        return chosen != 0u;
    }

    static void CheckDuty(double dutyCycle, string name)
    {
        // Written so that a NaN fails it too.
        if (!(dutyCycle >= 0.0 && dutyCycle <= 1.0))
        {
            throw new System.ArgumentOutOfRangeException(name);
        }
    }

    static void Check(bool done)
    {
        if (!done)
        {
            throw new System.InvalidOperationException(
                "the PWM counter did not finish a synchronized write; its generic clock is not running");
        }
    }

    // Eight hexadecimal digits, which is how a message names the counter: by its base address.
    static string Hex(uint value)
    {
        char[] digits = new char[8];
        for (int i = 7; i >= 0; i--)
        {
            uint nibble = value & 0xFu;
            digits[i] = (char)(nibble < 10u ? '0' + nibble : 'A' + (nibble - 10u));
            value = value >> 4;
        }
        return new string(digits);
    }
}

// One output of a Samd21PwmCounter, as the PwmChannel its owner holds.
internal sealed class Samd21PwmChannel : PwmChannel
{
    readonly Samd21PwmCounter _counter;
    readonly int _output;
    bool _disposed;

    internal Samd21PwmChannel(Samd21PwmCounter counter, int output)
    {
        _counter = counter;
        _output = output;
    }

    /// <summary>The counter's rate in hertz, which every output of it shares: the rate this channel
    /// asked for last, or the one another output set since while this one was stopped.</summary>
    /// <exception cref="System.ArgumentOutOfRangeException">The frequency is outside what the
    /// counter can produce.</exception>
    /// <exception cref="System.InvalidOperationException">Another output of the counter is started
    /// at a different frequency.</exception>
    public override int Frequency
    {
        get { return _counter.Rate; }
        set
        {
            CheckLive();
            _counter.SetFrequency(_output, value);
        }
    }

    /// <summary>The duty cycle this channel asked for, from 0.0 to 1.0.</summary>
    /// <exception cref="System.ArgumentOutOfRangeException">The duty cycle is outside 0.0 to
    /// 1.0.</exception>
    public override double DutyCycle
    {
        get { return _counter.DutyOf(_output); }
        set
        {
            CheckLive();
            _counter.SetDutyCycle(_output, value);
        }
    }

    public override void Start()
    {
        CheckLive();
        _counter.Start(_output);
    }

    public override void Stop()
    {
        if (!_disposed)
        {
            _counter.Stop(_output);
        }
    }

    protected override void Dispose(bool disposing)
    {
        if (!_disposed)
        {
            _counter.Release(_output);
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
