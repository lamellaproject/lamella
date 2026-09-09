// A System.Device.Gpio driver for the Microchip SAMC21, PA00..PA31 + PB00..PB31 + PC00..PC31.
using System.Device.Gpio;
using Lamella.Generated;
using Lamella.Hardware;

public sealed class Samc21GpioDriver : GpioDriver
{
    protected override int PinCount { get { return 96; } }

    protected override int ConvertPinNumberToLogicalNumberingScheme(int pinNumber) { return pinNumber; }

    protected override void OpenPin(int pinNumber) { }

    protected override void ClosePin(int pinNumber)
    {
        Mmio.Write32(GroupBase(pinNumber) + Samc21PortLayout.DIRCLR_OFF, PinMask(pinNumber));
        Mmio.Write8(PinCfgAddress(pinNumber), 0);
    }

    protected override void SetPinMode(int pinNumber, PinMode mode)
    {
        uint group = GroupBase(pinNumber);
        uint mask = PinMask(pinNumber);
        if (mode == PinMode.Output)
        {
            Mmio.Write8(PinCfgAddress(pinNumber), (byte)Samc21PortLayout.PINCFG0_INEN);
            Mmio.Write32(group + Samc21PortLayout.DIRSET_OFF, mask);
            return;
        }

        Mmio.Write32(group + Samc21PortLayout.DIRCLR_OFF, mask);
        if (mode == PinMode.InputPullUp)
        {
            Mmio.Write32(group + Samc21PortLayout.OUTSET_OFF, mask);
            Mmio.Write8(PinCfgAddress(pinNumber),
                (byte)(Samc21PortLayout.PINCFG0_INEN | Samc21PortLayout.PINCFG0_PULLEN));
            return;
        }
        if (mode == PinMode.InputPullDown)
        {
            Mmio.Write32(group + Samc21PortLayout.OUTCLR_OFF, mask);
            Mmio.Write8(PinCfgAddress(pinNumber),
                (byte)(Samc21PortLayout.PINCFG0_INEN | Samc21PortLayout.PINCFG0_PULLEN));
            return;
        }
        Mmio.Write8(PinCfgAddress(pinNumber), (byte)Samc21PortLayout.PINCFG0_INEN);
    }

    protected override PinMode GetPinMode(int pinNumber)
    {
        if ((Mmio.Read32(GroupBase(pinNumber) + Samc21PortLayout.DIR_OFF) & PinMask(pinNumber)) != 0u)
        {
            return PinMode.Output;
        }
        byte config = Mmio.Read8(PinCfgAddress(pinNumber));
        if ((config & (byte)Samc21PortLayout.PINCFG0_PULLEN) == 0)
        {
            return PinMode.Input;
        }
        uint level = Mmio.Read32(GroupBase(pinNumber) + Samc21PortLayout.OUT_OFF);
        return (level & PinMask(pinNumber)) != 0u ? PinMode.InputPullUp : PinMode.InputPullDown;
    }

    protected override bool IsPinModeSupported(int pinNumber, PinMode mode)
    {
        return mode == PinMode.Output || mode == PinMode.Input
            || mode == PinMode.InputPullUp || mode == PinMode.InputPullDown;
    }

    protected override PinValue Read(int pinNumber)
    {
        uint levels = Mmio.Read32(GroupBase(pinNumber) + Samc21PortLayout.IN_OFF);
        return (levels & PinMask(pinNumber)) != 0u ? PinValue.High : PinValue.Low;
    }

    protected override void Write(int pinNumber, PinValue value)
    {
        uint offset = (bool)value ? Samc21PortLayout.OUTSET_OFF : Samc21PortLayout.OUTCLR_OFF;
        Mmio.Write32(GroupBase(pinNumber) + offset, PinMask(pinNumber));
    }

    /// <summary>The logical pin number for a generated group base and pin index -- the form every
    /// board binding emits.</summary>
    /// <remarks>ON THE DRIVER RATHER THAN ON EACH BOARD, because which group a BASE is happens to be
    /// family truth: the bases come from this family's instance map and the pins-per-group is this
    /// family's, so each board class carrying its own switch would be one copy per board of one
    /// fact. What stays a BOARD fact is which base a device sits on, and that is what a board passes
    /// in -- which is what lets one driver serve a board whose LED is on group A and one whose LED
    /// is on group C without either class knowing the difference.</remarks>
    /// <exception cref="System.ArgumentException">The base is not a PORT group of this family.</exception>
    public static int LogicalPin(uint groupBase, uint pin)
    {
        if (groupBase == Samc21Instances.PORTA_BASE) return (int)pin;
        if (groupBase == Samc21Instances.PORTB_BASE) return 32 + (int)pin;
        if (groupBase == Samc21Instances.PORTC_BASE) return 64 + (int)pin;
#if LAMELLA_CORLIB_LINKED
        throw new System.ArgumentException("not a PORT group base of this family");
#else
        throw new System.Exception("not a PORT group base of this family");
#endif
    }

    static uint GroupBase(int pinNumber)
    {
        if (pinNumber < 32) return Samc21Instances.PORTA_BASE;
        if (pinNumber < 64) return Samc21Instances.PORTB_BASE;
        return Samc21Instances.PORTC_BASE;
    }

    static uint PinMask(int pinNumber)
    {
        return 1u << (pinNumber & 31);
    }

    static uint PinCfgAddress(int pinNumber)
    {
        return GroupBase(pinNumber) + Samc21PortLayout.PINCFG0_OFF + (uint)(pinNumber & 31);
    }

    /// <summary>Not supported: this driver does not implement pin-change events.</summary>
    protected override void AddCallbackForPinValueChangedEvent(
        int pinNumber, PinEventTypes eventTypes, PinChangeEventHandler callback)
    {
#if LAMELLA_CORLIB_LINKED
        throw new System.NotSupportedException("SAMC21 pin-change events are not implemented");
#else
        throw new System.Exception("SAMC21 pin-change events are not implemented");
#endif
    }

    /// <summary>Not supported: this driver does not implement pin-change events.</summary>
    protected override void RemoveCallbackForPinValueChangedEvent(int pinNumber, PinChangeEventHandler callback)
    {
#if LAMELLA_CORLIB_LINKED
        throw new System.NotSupportedException("SAMC21 pin-change events are not implemented");
#else
        throw new System.Exception("SAMC21 pin-change events are not implemented");
#endif
    }
}
