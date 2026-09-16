// A System.Device.Gpio driver for the Microchip SAMD21, PA00..PA31 + PB00..PB31. Subclasses the
using System.Device.Gpio;
using Lamella.Generated;
using Lamella.Hardware;

public sealed class Samd21GpioDriver : GpioDriver
{
    private PinChangeEventHandler[] _callbacks;
    private int[] _armedEdges;
    private int[] _lineHolder;

    protected override int PinCount { get { return 64; } }

    protected override int ConvertPinNumberToLogicalNumberingScheme(int pinNumber) { return pinNumber; }

    protected override void OpenPin(int pinNumber) { }

    protected override void ClosePin(int pinNumber)
    {
        if (IsArmed(pinNumber))
        {
            PinEvents.Unregister(pinNumber, _callbacks[pinNumber]);
            _callbacks[pinNumber] = null;
            Disarm(pinNumber);
        }
        Mmio.Write32(GroupBase(pinNumber) + Samd21PortLayout.DIRCLR_OFF, PinMask(pinNumber));
        Mmio.Write8(PinCfgAddress(pinNumber), 0);
    }

    protected override void SetPinMode(int pinNumber, PinMode mode)
    {
        uint group = GroupBase(pinNumber);
        uint mask = PinMask(pinNumber);
        if (mode == PinMode.Output)
        {
            if (IsArmed(pinNumber))
            {
                Invalid("a pin with pin-change callbacks cannot become an output; unregister them first");
            }
            Mmio.Write8(PinCfgAddress(pinNumber), (byte)Samd21PortLayout.PINCFG0_INEN);
            Mmio.Write32(group + Samd21PortLayout.DIRSET_OFF, mask);
            return;
        }

        uint muxed = IsArmed(pinNumber) ? Samd21PortLayout.PINCFG0_PMUXEN : 0u;
        Mmio.Write32(group + Samd21PortLayout.DIRCLR_OFF, mask);
        if (mode == PinMode.InputPullUp)
        {
            Mmio.Write32(group + Samd21PortLayout.OUTSET_OFF, mask);
            Mmio.Write8(PinCfgAddress(pinNumber),
                (byte)(Samd21PortLayout.PINCFG0_INEN | Samd21PortLayout.PINCFG0_PULLEN | muxed));
            return;
        }
        if (mode == PinMode.InputPullDown)
        {
            Mmio.Write32(group + Samd21PortLayout.OUTCLR_OFF, mask);
            Mmio.Write8(PinCfgAddress(pinNumber),
                (byte)(Samd21PortLayout.PINCFG0_INEN | Samd21PortLayout.PINCFG0_PULLEN | muxed));
            return;
        }
        Mmio.Write8(PinCfgAddress(pinNumber), (byte)(Samd21PortLayout.PINCFG0_INEN | muxed));
    }

    protected override PinMode GetPinMode(int pinNumber)
    {
        if ((Mmio.Read32(GroupBase(pinNumber) + Samd21PortLayout.DIR_OFF) & PinMask(pinNumber)) != 0u)
        {
            return PinMode.Output;
        }
        byte config = Mmio.Read8(PinCfgAddress(pinNumber));
        if ((config & (byte)Samd21PortLayout.PINCFG0_PULLEN) == 0)
        {
            return PinMode.Input;
        }
        uint level = Mmio.Read32(GroupBase(pinNumber) + Samd21PortLayout.OUT_OFF);
        return (level & PinMask(pinNumber)) != 0u ? PinMode.InputPullUp : PinMode.InputPullDown;
    }

    protected override bool IsPinModeSupported(int pinNumber, PinMode mode)
    {
        return mode == PinMode.Input || mode == PinMode.Output
            || mode == PinMode.InputPullUp || mode == PinMode.InputPullDown;
    }

    protected override PinValue Read(int pinNumber)
    {
        uint levels = Mmio.Read32(GroupBase(pinNumber) + Samd21PortLayout.IN_OFF);
        return (levels & PinMask(pinNumber)) != 0u ? PinValue.High : PinValue.Low;
    }

    protected override void Write(int pinNumber, PinValue value)
    {
        uint offset = (bool)value ? Samd21PortLayout.OUTSET_OFF : Samd21PortLayout.OUTCLR_OFF;
        Mmio.Write32(GroupBase(pinNumber) + offset, PinMask(pinNumber));
    }

    /// <summary>The logical pin number for a generated group base and pin index -- the form every
    /// board binding emits.</summary>
    /// <remarks>ON THE DRIVER RATHER THAN ON EACH BOARD, because which group a BASE is happens to be
    /// family truth: the bases come from this family's instance map and the pins-per-group is this
    /// family's, so each board class carrying its own switch would be one copy per board of one
    /// fact. What stays a BOARD fact is which base a device sits on, and that is what a board passes
    /// in.</remarks>
    /// <exception cref="System.ArgumentException">The base is not a PORT group of this family.</exception>
    public static int LogicalPin(uint groupBase, uint pin)
    {
        if (groupBase == Samd21Instances.PORTA_BASE) return (int)pin;
        if (groupBase == Samd21Instances.PORTB_BASE) return 32 + (int)pin;
#if LAMELLA_CORLIB_LINKED
        throw new System.ArgumentException("not a PORT group base of this family");
#else
        throw new System.Exception("not a PORT group base of this family");
#endif
    }

    /// <summary>Registers <paramref name="callback"/> for the edges in <paramref name="eventTypes"/>
    /// on a pin whose pad can raise an external interrupt.</summary>
    /// <remarks>The pin's callbacks start arriving once its pad is connected to its external
    /// interrupt line, which this call does. The firmware must already route the EIC's generic clock
    /// and enable the EIC's interrupt. A refused registration leaves every pin unarmed.</remarks>
    /// <exception cref="System.ArgumentException"><paramref name="eventTypes"/> names neither a rising
    /// nor a falling edge.</exception>
    /// <exception cref="System.NotSupportedException">The pad raises no external interrupt, or raises
    /// the non-maskable interrupt, which cannot serve a pin-change event.</exception>
    /// <exception cref="System.InvalidOperationException">The EIC's generic clock is not running, the
    /// EIC did not finish synchronizing, or another pin already holds the external interrupt line this
    /// pad raises.</exception>
    protected override void AddCallbackForPinValueChangedEvent(
        int pinNumber, PinEventTypes eventTypes, PinChangeEventHandler callback)
    {
        int edges = (int)eventTypes & ((int)PinEventTypes.Rising | (int)PinEventTypes.Falling);
        if (edges == 0)
        {
            BadArgument("eventTypes names neither a rising nor a falling edge");
        }
        int line = LineOf(pinNumber);
        if (line == Samd21Pins.EXTINT_NMI)
        {
            Unsupported("this pad raises the non-maskable interrupt, which cannot serve a pin-change event");
        }
        if (line < 0)
        {
            Unsupported("this pad raises no external interrupt");
        }
        if (_lineHolder == null)
        {
            _callbacks = new PinChangeEventHandler[PinCount];
            _armedEdges = new int[PinCount];
            _lineHolder = new int[(int)Samd21EicLayout.LINE_COUNT];
            for (int i = 0; i < _lineHolder.Length; i = i + 1)
            {
                _lineHolder[i] = -1;
            }
        }
        if (_lineHolder[line] >= 0 && _lineHolder[line] != pinNumber)
        {
            Invalid("another pin already holds the external interrupt line this pad raises");
        }

        PinEvents.Register(this, pinNumber, pinNumber, eventTypes, callback);

        int wanted = _armedEdges[pinNumber] | edges;
        if (IsArmed(pinNumber))
        {
            _callbacks[pinNumber] = (PinChangeEventHandler)System.Delegate.Combine(_callbacks[pinNumber], callback);
            if (wanted != _armedEdges[pinNumber])
            {
                _armedEdges[pinNumber] = wanted;
                WriteSense(line, wanted);
            }
            return;
        }

        string refusal = ArmLine(pinNumber, line, wanted);
        if ((object)refusal != null)
        {
            PinEvents.Unregister(pinNumber, callback);
            Invalid(refusal);
        }
        _callbacks[pinNumber] = callback;
        _armedEdges[pinNumber] = wanted;
        _lineHolder[line] = pinNumber;
    }

    /// <summary>Removes <paramref name="callback"/> from the pin, and releases the pad's external
    /// interrupt line once no callback remains on it.</summary>
    protected override void RemoveCallbackForPinValueChangedEvent(int pinNumber, PinChangeEventHandler callback)
    {
        PinEvents.Unregister(pinNumber, callback);
        if (!IsArmed(pinNumber))
        {
            return;
        }
        _callbacks[pinNumber] = (PinChangeEventHandler)System.Delegate.Remove(_callbacks[pinNumber], callback);
        if ((object)_callbacks[pinNumber] == null)
        {
            Disarm(pinNumber);
        }
    }

    static uint GroupBase(int pinNumber)
    {
        return pinNumber < 32 ? Samd21Instances.PORTA_BASE : Samd21Instances.PORTB_BASE;
    }

    static uint PinMask(int pinNumber)
    {
        return 1u << (pinNumber & 31);
    }

    static uint PinCfgAddress(int pinNumber)
    {
        return GroupBase(pinNumber) + Samd21PortLayout.PINCFG0_OFF + (uint)(pinNumber & 31);
    }

    static int LineOf(int pinNumber)
    {
        return Samd21Pins.ExtIntLine(pinNumber / 32, pinNumber & 31);
    }

    bool IsArmed(int pinNumber)
    {
        return _callbacks != null && pinNumber >= 0 && pinNumber < PinCount
            && (object)_callbacks[pinNumber] != null;
    }

    void Disarm(int pinNumber)
    {
        int line = LineOf(pinNumber);
        uint bit = 1u << line;
        Mmio.Write32(Samd21Instances.EIC_BASE + Samd21EicLayout.INTENCLR_OFF, bit);
        bool high = (bool)Read(pinNumber);
        if (high)
        {
            WriteSense(line, (int)PinEventTypes.Falling);
        }
        uint pincfg = PinCfgAddress(pinNumber);
        Mmio.Write8(pincfg, (byte)(Mmio.Read8(pincfg) & ~Samd21PortLayout.PINCFG0_PMUXEN));
        if (high)
        {
            WaitForFlag(bit);
        }
        Mmio.Write32(Samd21Instances.EIC_BASE + Samd21EicLayout.INTFLAG_OFF, bit);
        WriteSense(line, 0);
        _armedEdges[pinNumber] = 0;
        _lineHolder[line] = -1;
    }

    static void MuxToEic(int pinNumber)
    {
        int index = pinNumber & 31;
        uint pmux = GroupBase(pinNumber) + Samd21PortLayout.PMUX0_OFF + (uint)(index / 2);
        uint nibble = Samd21PortLayout.PMUX0_PMUXE;
        int lsb = (int)Samd21PortLayout.PMUX0_PMUXE_LSB;
        if ((index & 1) != 0)
        {
            nibble = Samd21PortLayout.PMUX0_PMUXO;
            lsb = (int)Samd21PortLayout.PMUX0_PMUXO_LSB;
        }
        uint current = Mmio.Read8(pmux);
        Mmio.Write8(pmux, (byte)((current & ~nibble) | (Samd21Pins.EXTINT_FUNCTION << lsb)));
        uint pincfg = PinCfgAddress(pinNumber);
        Mmio.Write8(pincfg,
            (byte)(Mmio.Read8(pincfg) | Samd21PortLayout.PINCFG0_PMUXEN | Samd21PortLayout.PINCFG0_INEN));
    }

    static void WriteSense(int line, int edges)
    {
        bool rising = (edges & (int)PinEventTypes.Rising) != 0;
        bool falling = (edges & (int)PinEventTypes.Falling) != 0;
        uint sense = Samd21EicLayout.SENSE_NONE;
        if (rising && falling)
        {
            sense = Samd21EicLayout.SENSE_BOTH;
        }
        else if (rising)
        {
            sense = Samd21EicLayout.SENSE_RISE;
        }
        else if (falling)
        {
            sense = Samd21EicLayout.SENSE_FALL;
        }
        int stride = (int)(Samd21EicLayout.CONFIG_SENSE1_LSB - Samd21EicLayout.CONFIG_SENSE0_LSB);
        int perRegister = Samd21EicLayout.CONFIG_WIDTH / stride;
        uint address = Samd21Instances.EIC_BASE + Samd21EicLayout.CONFIG_OFF
            + (uint)((line / perRegister) * (Samd21EicLayout.CONFIG_WIDTH / 8));
        int shift = (line % perRegister) * stride;
        uint field = (Samd21EicLayout.CONFIG_SENSE0 | Samd21EicLayout.CONFIG_FILTEN0) << shift;
        Mmio.Write32(address, (Mmio.Read32(address) & ~field) | (sense << shift));
    }

    string ArmLine(int pinNumber, int line, int edges)
    {
        if (!SetEicEnabled(true))
        {
            return "pin-change events need the EIC's generic clock (GCLK_EIC) running, and the EIC did not finish synchronizing";
        }
        WriteSense(line, edges);
        if (!EicClockRunning())
        {
            WriteSense(line, 0);
            if (!AnyLineHeld())
            {
                SetEicEnabled(false);
            }
            return "pin-change events need the EIC's generic clock (GCLK_EIC) running, and it is not enabled";
        }
        uint bit = 1u << line;
        Mmio.Write32(Samd21Instances.EIC_BASE + Samd21EicLayout.INTFLAG_OFF, bit);
        MuxToEic(pinNumber);
        if ((edges & (int)PinEventTypes.Rising) != 0 && (bool)Read(pinNumber))
        {
            WaitForFlag(bit);
        }
        Mmio.Write32(Samd21Instances.EIC_BASE + Samd21EicLayout.INTFLAG_OFF, bit);
        Mmio.Write32(Samd21Instances.EIC_BASE + Samd21EicLayout.INTENSET_OFF, bit);
        return null;
    }

    bool AnyLineHeld()
    {
        for (int i = 0; i < _lineHolder.Length; i = i + 1)
        {
            if (_lineHolder[i] >= 0)
            {
                return true;
            }
        }
        return false;
    }

    static bool SetEicEnabled(bool enabled)
    {
        if (!EicSynchronized())
        {
            return false;
        }
        uint ctrl = Samd21Instances.EIC_BASE + Samd21EicLayout.CTRL_OFF;
        uint current = Mmio.Read8(ctrl);
        uint next = enabled ? (current | Samd21EicLayout.CTRL_ENABLE) : (current & ~Samd21EicLayout.CTRL_ENABLE);
        if (next == current)
        {
            return true;
        }
        Mmio.Write8(ctrl, (byte)next);
        return EicSynchronized();
    }

    const int WaitBound = 100000;

    static bool EicSynchronized()
    {
        uint status = Samd21Instances.EIC_BASE + Samd21EicLayout.STATUS_OFF;
        for (int polls = 0; polls < WaitBound; polls = polls + 1)
        {
            if ((Mmio.Read8(status) & Samd21EicLayout.STATUS_SYNCBUSY) == 0u)
            {
                return true;
            }
        }
        return false;
    }

    static bool WaitForFlag(uint bit)
    {
        uint intflag = Samd21Instances.EIC_BASE + Samd21EicLayout.INTFLAG_OFF;
        for (int polls = 0; polls < WaitBound; polls = polls + 1)
        {
            if ((Mmio.Read32(intflag) & bit) != 0u)
            {
                return true;
            }
        }
        return false;
    }

    static bool EicClockRunning()
    {
        uint clkctrl = Samd21Instances.GCLK_BASE + Samd21GclkLayout.CLKCTRL_OFF;
        Mmio.Write8(clkctrl, (byte)Samd21Instances.EIC_GCLK_CORE_ID);
        return (Mmio.Read16(clkctrl) & Samd21GclkLayout.CLKCTRL_CLKEN) != 0u;
    }

    static void BadArgument(string why)
    {
#if LAMELLA_CORLIB_LINKED
        throw new System.ArgumentException(why);
#else
        throw new System.Exception(why);
#endif
    }

    static void Unsupported(string why)
    {
#if LAMELLA_CORLIB_LINKED
        throw new System.NotSupportedException(why);
#else
        throw new System.Exception(why);
#endif
    }

    static void Invalid(string why)
    {
#if LAMELLA_CORLIB_LINKED
        throw new System.InvalidOperationException(why);
#else
        throw new System.Exception(why);
#endif
    }
}
