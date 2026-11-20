// A System.Device.Gpio driver for the GPIO ports of an STM32F7 or STM32U5 part, over Lamella.Hardware.Mmio.
using System.Device.Gpio;
using Lamella.Hardware;

/// <summary>Drives the GPIO ports of an STM32F7 or STM32U5 part. A pin is numbered by its port and its
/// index in the port, port * 16 + index: PA0 is 0, PB0 is 16 and PJ13 is 157.</summary>
internal sealed class Stm32GpioDriver : GpioDriver
{
    const int PinsPerPort = 16;
    const string PortLetters = "ABCDEFGHIJKLMNOP";

    readonly Stm32GpioPorts _ports;
    readonly Stm32ExtiLines _lines;

    readonly uint[] _pads;

    readonly uint[] _inputOnly;

    /// <summary>A driver for the ports <paramref name="ports"/> places, arming their pins' lines through
    /// <paramref name="lines"/>, over the pads <paramref name="pads"/> names: one word a port, port A
    /// first, whose low sixteen bits mark the pads the part's package bonds and whose high sixteen bits
    /// mark the pads the board keeps, so that no program reconfigures them. <paramref name="inputOnly"/>
    /// marks the pads the board holds to input, a bit each in one word a port, so that no program makes
    /// them outputs; null when the board holds none.</summary>
    /// <exception cref="System.ArgumentException"><paramref name="pads"/> or <paramref name="inputOnly"/>
    /// does not hold one word a port, or <paramref name="pads"/> marks a pad the board keeps that the
    /// package does not bond.</exception>
    internal Stm32GpioDriver(Stm32GpioPorts ports, Stm32ExtiLines lines, uint[] pads, uint[] inputOnly)
    {
        if (pads.Length != ports.Bases.Length || ((object)inputOnly != null && inputOnly.Length != pads.Length))
        {
            throw new System.ArgumentException("the pad table needs exactly one word a port");
        }
        for (int port = 0; port < pads.Length; port++)
        {
            if (((pads[port] >> 16) & ~pads[port] & 0xFFFFu) != 0u)
            {
                throw new System.ArgumentException("the board keeps a pad of port " + PortLetters.Substring(port, 1)
                    + " that the part's package does not bond");
            }
        }
        _ports = ports;
        _lines = lines;
        _pads = pads;
        _inputOnly = inputOnly;
    }

    protected override int PinCount
    {
        get { return _pads.Length * PinsPerPort; }
    }

    /// <exception cref="System.ArgumentOutOfRangeException"><paramref name="pinNumber"/> names no pad of this
    /// part's package.</exception>
    protected override void OpenPin(int pinNumber)
    {
        CheckPad(pinNumber);
    }

    protected override void ClosePin(int pinNumber)
    {
        if (!IsPad(pinNumber) || IsKept(pinNumber)) return;

        if (IsArmed(pinNumber))
        {
            for (int edges = 1; edges <= EdgeSets; edges = edges + 1)
            {
                PinChangeEventHandler row = _rows[RowOf(pinNumber, edges)];
                if ((object)row != null)
                {
                    PinEvents.Unregister(pinNumber, row);
                    RemoveFromRows(pinNumber, row);
                }
            }
            Sense(pinNumber, 0);
        }
        int port = pinNumber / PinsPerPort;
        int index = pinNumber % PinsPerPort;
        EnsurePortClocked(port);
        WriteTwoBits(port, _ports.Moder, index, _ports.ModeReset);
        WriteTwoBits(port, _ports.Pupdr, index, _ports.PullNone);
    }

    /// <exception cref="System.ArgumentOutOfRangeException"><paramref name="pinNumber"/> names no pad of this
    /// part's package.</exception>
    /// <exception cref="System.ArgumentException">The board keeps the pin.</exception>
    /// <exception cref="System.InvalidOperationException"><paramref name="mode"/> is an output, and the board
    /// holds the pin to input.</exception>
    protected override void SetPinMode(int pinNumber, PinMode mode)
    {
        CheckPad(pinNumber);
        CheckNotKept(pinNumber);
        if (mode == PinMode.Output && IsInputOnly(pinNumber))
        {
            throw new System.InvalidOperationException("pin " + Named(pinNumber) + " is held to input by the board");
        }
        int port = pinNumber / PinsPerPort;
        int index = pinNumber % PinsPerPort;
        EnsurePortClocked(port);
        uint pull = _ports.PullNone;
        if (mode == PinMode.InputPullUp) pull = _ports.PullUp;
        else if (mode == PinMode.InputPullDown) pull = _ports.PullDown;
        WriteTwoBits(port, _ports.Pupdr, index, pull);
        if (mode == PinMode.Output)
        {
            uint otyper = _ports.Bases[port] + _ports.Otyper;
            Mmio.Write32(otyper, Mmio.Read32(otyper) & ~(1u << index));
            WriteTwoBits(port, _ports.Moder, index, _ports.ModeOutput);
        }
        else
        {
            WriteTwoBits(port, _ports.Moder, index, _ports.ModeInput);
        }
    }

    /// <exception cref="System.ArgumentOutOfRangeException"><paramref name="pinNumber"/> names no pad of this
    /// part's package.</exception>
    protected override PinMode GetPinMode(int pinNumber)
    {
        CheckPad(pinNumber);
        int port = pinNumber / PinsPerPort;
        int index = pinNumber % PinsPerPort;
        if (ReadTwoBits(port, _ports.Moder, index) == _ports.ModeOutput) return PinMode.Output;
        uint pull = ReadTwoBits(port, _ports.Pupdr, index);
        if (pull == _ports.PullUp) return PinMode.InputPullUp;
        if (pull == _ports.PullDown) return PinMode.InputPullDown;
        return PinMode.Input;
    }

    /// <summary>Whether a program can set the pin to <paramref name="mode"/>: every mode, on a pad the
    /// part's package bonds and the board does not keep, but an output on a pad the board holds to
    /// input.</summary>
    protected override bool IsPinModeSupported(int pinNumber, PinMode mode)
    {
        return IsPad(pinNumber) && !IsKept(pinNumber) && !(mode == PinMode.Output && IsInputOnly(pinNumber));
    }

    /// <exception cref="System.ArgumentOutOfRangeException"><paramref name="pinNumber"/> names no pad of this
    /// part's package.</exception>
    protected override PinValue Read(int pinNumber)
    {
        CheckPad(pinNumber);
        uint idr = Mmio.Read32(_ports.Bases[pinNumber / PinsPerPort] + _ports.Idr);
        return (int)((idr >> (pinNumber % PinsPerPort)) & 1u);
    }

    /// <exception cref="System.ArgumentOutOfRangeException"><paramref name="pinNumber"/> names no pad of this
    /// part's package.</exception>
    protected override void Write(int pinNumber, PinValue value)
    {
        CheckPad(pinNumber);
        int index = pinNumber % PinsPerPort;
        uint bit = (bool)value ? 1u << index : 1u << (index + 16);
        Mmio.Write32(_ports.Bases[pinNumber / PinsPerPort] + _ports.Bsrr, bit);
    }


    /// <summary>Registers <paramref name="callback"/> for the edges in <paramref name="eventTypes"/> on the
    /// pin, and arms the pin's external interrupt line. A pin in analog mode is made an input first, so
    /// its input buffer reads the pad.</summary>
    /// <exception cref="System.ArgumentOutOfRangeException"><paramref name="pinNumber"/> names no pad of this
    /// part's package.</exception>
    /// <exception cref="System.ArgumentException">The board keeps the pin, or <paramref name="eventTypes"/>
    /// names neither a rising nor a falling edge.</exception>
    /// <exception cref="System.InvalidOperationException">A pin of the same index on another port holds the
    /// line: one line serves that index on every port, and one pin at a time. The message names both
    /// pins, and nothing is changed.</exception>
    protected override void AddCallbackForPinValueChangedEvent(
        int pinNumber, PinEventTypes eventTypes, PinChangeEventHandler callback)
    {
        CheckPad(pinNumber);
        CheckNotKept(pinNumber);
        int edges = (int)eventTypes & Both;
        if (edges == 0)
        {
            throw new System.ArgumentException("eventTypes names neither a rising nor a falling edge");
        }
        int holder = LineHolder(pinNumber % PinsPerPort);
        if (holder >= 0 && holder != pinNumber)
        {
            throw new System.InvalidOperationException("pin " + Named(pinNumber) + " shares its interrupt line with pin "
                + Named(holder) + ", whose callbacks hold it: remove them, or close that pin, first");
        }
        PinEvents.Register(this, pinNumber, pinNumber, (PinEventTypes)edges, callback);
        if (_rows == null)
        {
            _rows = new PinChangeEventHandler[PinCount * EdgeSets];
            _armed = new int[PinCount];
        }
        int row = RowOf(pinNumber, edges);
        _rows[row] = (PinChangeEventHandler)System.Delegate.Combine(_rows[row], callback);
        Sense(pinNumber, _armed[pinNumber] | edges);
    }

    /// <summary>Removes <paramref name="callback"/> from the pin, for every edge it was registered for, and
    /// disarms the pin's line once no callback remains on it.</summary>
    /// <exception cref="System.ArgumentOutOfRangeException"><paramref name="pinNumber"/> names no pad of this
    /// part's package.</exception>
    protected override void RemoveCallbackForPinValueChangedEvent(int pinNumber, PinChangeEventHandler callback)
    {
        CheckPad(pinNumber);
        PinEvents.Unregister(pinNumber, callback);
        if (!IsArmed(pinNumber))
        {
            return;
        }
        RemoveFromRows(pinNumber, callback);
        Sense(pinNumber, EdgesInRows(pinNumber));
    }

    private PinChangeEventHandler[] _rows;
    private int[] _armed;
    const int EdgeSets = 3;
    const int Both = (int)PinEventTypes.Rising | (int)PinEventTypes.Falling;

    static int RowOf(int pinNumber, int edges)
    {
        return pinNumber * EdgeSets + edges - 1;
    }

    bool IsArmed(int pinNumber)
    {
        return _armed != null && pinNumber >= 0 && pinNumber < _armed.Length && _armed[pinNumber] != 0;
    }

    void RemoveFromRows(int pinNumber, PinChangeEventHandler callback)
    {
        for (int edges = 1; edges <= EdgeSets; edges = edges + 1)
        {
            int row = RowOf(pinNumber, edges);
            _rows[row] = (PinChangeEventHandler)System.Delegate.Remove(_rows[row], callback);
        }
    }

    int EdgesInRows(int pinNumber)
    {
        int edges = 0;
        for (int set = 1; set <= EdgeSets; set = set + 1)
        {
            if ((object)_rows[RowOf(pinNumber, set)] != null)
            {
                edges = edges | set;
            }
        }
        return edges;
    }

    int LineHolder(int line)
    {
        if ((Mmio.Read32(_lines.Mask) & (1u << line)) == 0u)
        {
            return -1;
        }
        EnsureSelectorClocked();
        uint selector = _lines.Selectors + (uint)(line / 4) * 4u;
        int shift = (line % 4) * _lines.SelectorBits;
        int port = (int)((Mmio.Read32(selector) >> shift) & ((1u << _lines.SelectorBits) - 1u));
        return port * PinsPerPort + line;
    }

    void Sense(int pinNumber, int edges)
    {
        int was = _armed[pinNumber];
        if (edges == was)
        {
            return;
        }
        int line = pinNumber % PinsPerPort;
        uint bit = 1u << line;
        if (edges == 0)
        {
            Assign(_lines.Mask, bit, false);
            Assign(_lines.RisingTrigger, bit, false);
            Assign(_lines.FallingTrigger, bit, false);
            ClearPending(bit);
            _armed[pinNumber] = 0;
            return;
        }
        if (was == 0)
        {
            int port = pinNumber / PinsPerPort;
            EnsurePortClocked(port);
            if (ReadTwoBits(port, _ports.Moder, line) == _ports.ModeAnalog)
            {
                WriteTwoBits(port, _ports.Moder, line, _ports.ModeInput);
            }
            EnsureSelectorClocked();
            uint selector = _lines.Selectors + (uint)(line / 4) * 4u;
            int shift = (line % 4) * _lines.SelectorBits;
            uint field = ((1u << _lines.SelectorBits) - 1u) << shift;
            Mmio.Write32(selector, (Mmio.Read32(selector) & ~field) | ((uint)port << shift));
            ClearPending(bit);
        }
        Assign(_lines.RisingTrigger, bit, (edges & (int)PinEventTypes.Rising) != 0);
        Assign(_lines.FallingTrigger, bit, (edges & (int)PinEventTypes.Falling) != 0);
        Assign(_lines.Mask, bit, true);
        _armed[pinNumber] = edges;
    }

    void ClearPending(uint bit)
    {
        Mmio.Write32(_lines.RisingPending, bit);
        if (_lines.FallingPending != _lines.RisingPending)
        {
            Mmio.Write32(_lines.FallingPending, bit);
        }
    }

    static void Assign(uint register, uint bit, bool set)
    {
        uint value = Mmio.Read32(register);
        Mmio.Write32(register, set ? value | bit : value & ~bit);
    }

    void EnsurePortClocked(int port)
    {
        Enable(_ports.ClockRegisters[port], _ports.ClockMasks[port]);
    }

    void EnsureSelectorClocked()
    {
        if (_lines.SelectorClockMask != 0u)
        {
            Enable(_lines.SelectorClockRegister, _lines.SelectorClockMask);
        }
    }

    static void Enable(uint register, uint mask)
    {
        uint current = Mmio.Read32(register);
        if ((current & mask) == mask) return;
        Mmio.Write32(register, current | mask);
        Mmio.Read32(register);
    }

    void WriteTwoBits(int port, uint offset, int index, uint value)
    {
        uint register = _ports.Bases[port] + offset;
        int shift = index * 2;
        Mmio.Write32(register, (Mmio.Read32(register) & ~(3u << shift)) | (value << shift));
    }

    uint ReadTwoBits(int port, uint offset, int index)
    {
        return (Mmio.Read32(_ports.Bases[port] + offset) >> (index * 2)) & 3u;
    }

    bool IsPad(int pinNumber)
    {
        if ((uint)pinNumber >= (uint)(_pads.Length * PinsPerPort)) return false;
        return ((_pads[pinNumber / PinsPerPort] >> (pinNumber % PinsPerPort)) & 1u) != 0u;
    }

    bool IsKept(int pinNumber)
    {
        return ((_pads[pinNumber / PinsPerPort] >> (16 + pinNumber % PinsPerPort)) & 1u) != 0u;
    }

    bool IsInputOnly(int pinNumber)
    {
        return (object)_inputOnly != null && ((_inputOnly[pinNumber / PinsPerPort] >> (pinNumber % PinsPerPort)) & 1u) != 0u;
    }

    void CheckPad(int pinNumber)
    {
        if (IsPad(pinNumber)) return;
        if ((uint)pinNumber >= (uint)(_pads.Length * PinsPerPort))
        {
            throw new System.ArgumentOutOfRangeException("pinNumber", "This part's GPIO ports number their pins 0 to "
                + (_pads.Length * PinsPerPort - 1).ToString() + ", sixteen a port, port A's first.");
        }
        throw new System.ArgumentOutOfRangeException("pinNumber", "Pin " + Named(pinNumber)
            + " is not a pad of this part's package.");
    }

    void CheckNotKept(int pinNumber)
    {
        if (IsKept(pinNumber))
        {
            throw new System.ArgumentException("pin " + Named(pinNumber) + " is reserved by the board");
        }
    }

    static string Named(int pinNumber)
    {
        int port = pinNumber / PinsPerPort;
        if (port >= PortLetters.Length) return pinNumber.ToString();
        return pinNumber.ToString() + " (P" + PortLetters.Substring(port, 1)
            + (pinNumber % PinsPerPort).ToString() + ")";
    }
}
