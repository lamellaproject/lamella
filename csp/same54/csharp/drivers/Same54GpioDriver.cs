// A System.Device.Gpio driver for the SAM E54's four PORT groups, PA00 through PD31, over
using System.Device.Gpio;
using Lamella.Generated;
using Lamella.Hardware;

public sealed class Same54GpioDriver : GpioDriver
{
    private const int PinsPerGroup = 32;
    private const int GroupCount = 4;

    private readonly uint _reservedA;
    private readonly uint _reservedB;

    /// <summary>Reserves nothing -- every pin is the app's.</summary>
    public Same54GpioDriver() : this(0u, 0u) { }

    /// <summary>Reserves the pins whose bits are set, so <c>SetPinMode</c> refuses them and
    /// <c>ClosePin</c> leaves them alone. A board passes the lines it must keep -- its debug
    /// transport, a fixed peripheral's control pin -- because only the board knows which those
    /// are.</summary>
    /// <remarks>TWO MASKS, COVERING PA00 THROUGH PB31 ONLY, AND THE DEBUG PINS ARE INSIDE THEM.
    /// SWCLK is PA30 and SWDIO is PA31. The manual notes that only SWCLK is mapped to the normal
    /// PORT functions and that a debugger's plug detection switches SWDIO to its debug function by
    /// itself -- so SWCLK is the one an ordinary GPIO call can genuinely take, and it is bit 30 of
    /// the first mask. A board wanting to reserve a pin above PB31 is asking for something this
    /// shape cannot express and should say so rather than be given a silent half-answer.</remarks>
    public Same54GpioDriver(uint reservedA, uint reservedB)
    {
        _reservedA = reservedA;
        _reservedB = reservedB;
    }

    protected override int PinCount { get { return GroupCount * PinsPerGroup; } }

    protected override int ConvertPinNumberToLogicalNumberingScheme(int pinNumber) { return pinNumber; }

    protected override void OpenPin(int pinNumber) { }

    protected override void ClosePin(int pinNumber)
    {
        if (IsReserved(pinNumber)) return;

        Mmio.Write32(GroupBase(pinNumber) + Same54PortLayout.DIRCLR_OFF, PinMask(pinNumber));
        Mmio.Write8(PinCfgAddress(pinNumber), 0);
    }

    private bool IsReserved(int pinNumber)
    {
        if (pinNumber < 32) return (_reservedA & (1u << pinNumber)) != 0u;
        if (pinNumber < 64) return (_reservedB & (1u << (pinNumber - 32))) != 0u;
        return false;
    }

    /// <summary>The logical pin number for a generated group base and pin index -- the form every
    /// board binding emits.</summary>
    /// <remarks>ON THE DRIVER RATHER THAN ON EACH BOARD, because which group a BASE is happens to be
    /// family truth: the bases come from this family's instance map and the pins-per-group is this
    /// family's, so each board class carrying a four-way switch would be one copy per board of one
    /// fact. What stays a BOARD fact is which base a device sits on, and that is what a board passes
    /// in.</remarks>
    /// <exception cref="System.ArgumentException">The base is not a PORT group of this family.</exception>
    public static int LogicalPin(uint groupBase, uint pin)
    {
        for (int group = 0; group < GroupCount; group++)
        {
            if (GroupBaseOf(group) == groupBase) return group * PinsPerGroup + (int)pin;
        }
#if LAMELLA_CORLIB_LINKED
        throw new System.ArgumentException("not a PORT group base of this family");
#else
        throw new System.Exception("not a PORT group base of this family");
#endif
    }

    protected override void SetPinMode(int pinNumber, PinMode mode)
    {
        if (IsReserved(pinNumber))
        {
#if LAMELLA_CORLIB_LINKED
            throw new System.ArgumentException("pin is reserved by the board");
#else
            throw new System.Exception("pin is reserved by the board");
#endif
        }

        uint group = GroupBase(pinNumber);
        uint mask = PinMask(pinNumber);
        if (mode == PinMode.Output)
        {
            Mmio.Write8(PinCfgAddress(pinNumber), (byte)Same54PortLayout.PINCFG_INEN);
            Mmio.Write32(group + Same54PortLayout.DIRSET_OFF, mask);
            return;
        }

        Mmio.Write32(group + Same54PortLayout.DIRCLR_OFF, mask);
        if (mode == PinMode.InputPullUp)
        {
            Mmio.Write32(group + Same54PortLayout.OUTSET_OFF, mask);
            Mmio.Write8(PinCfgAddress(pinNumber),
                (byte)(Same54PortLayout.PINCFG_INEN | Same54PortLayout.PINCFG_PULLEN));
            return;
        }
        if (mode == PinMode.InputPullDown)
        {
            Mmio.Write32(group + Same54PortLayout.OUTCLR_OFF, mask);
            Mmio.Write8(PinCfgAddress(pinNumber),
                (byte)(Same54PortLayout.PINCFG_INEN | Same54PortLayout.PINCFG_PULLEN));
            return;
        }
        Mmio.Write8(PinCfgAddress(pinNumber), (byte)Same54PortLayout.PINCFG_INEN);
    }

    protected override PinMode GetPinMode(int pinNumber)
    {
        if ((Mmio.Read32(GroupBase(pinNumber) + Same54PortLayout.DIR_OFF) & PinMask(pinNumber)) != 0u)
        {
            return PinMode.Output;
        }
        byte config = Mmio.Read8(PinCfgAddress(pinNumber));
        if ((config & (byte)Same54PortLayout.PINCFG_PULLEN) == 0)
        {
            return PinMode.Input;
        }
        uint level = Mmio.Read32(GroupBase(pinNumber) + Same54PortLayout.OUT_OFF);
        return (level & PinMask(pinNumber)) != 0u ? PinMode.InputPullUp : PinMode.InputPullDown;
    }

    protected override bool IsPinModeSupported(int pinNumber, PinMode mode)
    {
        return mode == PinMode.Input || mode == PinMode.Output
            || mode == PinMode.InputPullUp || mode == PinMode.InputPullDown;
    }

    protected override PinValue Read(int pinNumber)
    {
        uint levels = Mmio.Read32(GroupBase(pinNumber) + Same54PortLayout.IN_OFF);
        return (levels & PinMask(pinNumber)) != 0u ? PinValue.High : PinValue.Low;
    }

    protected override void Write(int pinNumber, PinValue value)
    {
        uint offset = (bool)value ? Same54PortLayout.OUTSET_OFF : Same54PortLayout.OUTCLR_OFF;
        Mmio.Write32(GroupBase(pinNumber) + offset, PinMask(pinNumber));
    }

    protected override void Toggle(int pinNumber)
    {
        Mmio.Write32(GroupBase(pinNumber) + Same54PortLayout.OUTTGL_OFF, PinMask(pinNumber));
    }

    private static uint GroupBaseOf(int group)
    {
        switch (group)
        {
            case 0: return Same54Instances.PORTA_BASE;
            case 1: return Same54Instances.PORTB_BASE;
            case 2: return Same54Instances.PORTC_BASE;
            default: return Same54Instances.PORTD_BASE;
        }
    }

    private static uint GroupBase(int pinNumber) { return GroupBaseOf(pinNumber / PinsPerGroup); }

    private static uint PinMask(int pinNumber) { return 1u << (pinNumber & 31); }

    private static uint PinCfgAddress(int pinNumber)
    {
        return GroupBase(pinNumber) + Same54PortLayout.PINCFG0_OFF + (uint)(pinNumber & 31);
    }

    /// <summary>Not supported: this driver does not implement pin-change events.</summary>
    protected override void AddCallbackForPinValueChangedEvent(
        int pinNumber, PinEventTypes eventTypes, PinChangeEventHandler callback)
    {
#if LAMELLA_CORLIB_LINKED
        throw new System.NotSupportedException("SAM E54 pin-change events are not implemented");
#else
        throw new System.Exception("SAM E54 pin-change events are not implemented");
#endif
    }

    /// <summary>Not supported: this driver does not implement pin-change events.</summary>
    protected override void RemoveCallbackForPinValueChangedEvent(int pinNumber, PinChangeEventHandler callback)
    {
#if LAMELLA_CORLIB_LINKED
        throw new System.NotSupportedException("SAM E54 pin-change events are not implemented");
#else
        throw new System.Exception("SAM E54 pin-change events are not implemented");
#endif
    }
}
