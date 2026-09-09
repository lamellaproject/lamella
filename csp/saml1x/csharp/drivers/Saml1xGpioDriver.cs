// A System.Device.Gpio driver for the Microchip SAM L10 and SAM L11, PA00..PA31.
using System.Device.Gpio;
using Lamella.Generated;
using Lamella.Hardware;

public sealed class Saml1xGpioDriver : GpioDriver
{
    protected override int PinCount { get { return 32; } }

    protected override int ConvertPinNumberToLogicalNumberingScheme(int pinNumber) { return pinNumber; }

    protected override void OpenPin(int pinNumber) { }

    protected override void ClosePin(int pinNumber)
    {
        Mmio.Write32(GroupBase(pinNumber) + Saml1xPortLayout.DIRCLR_OFF, PinMask(pinNumber));
        Mmio.Write8(PinCfgAddress(pinNumber), 0);
    }

    protected override void SetPinMode(int pinNumber, PinMode mode)
    {
        uint group = GroupBase(pinNumber);
        uint mask = PinMask(pinNumber);
        if (mode == PinMode.Output)
        {
            Mmio.Write8(PinCfgAddress(pinNumber), (byte)Saml1xPortLayout.PINCFG0_INEN);
            Mmio.Write32(group + Saml1xPortLayout.DIRSET_OFF, mask);
            return;
        }

        Mmio.Write32(group + Saml1xPortLayout.DIRCLR_OFF, mask);
        if (mode == PinMode.InputPullUp)
        {
            Mmio.Write32(group + Saml1xPortLayout.OUTSET_OFF, mask);
            Mmio.Write8(PinCfgAddress(pinNumber),
                (byte)(Saml1xPortLayout.PINCFG0_INEN | Saml1xPortLayout.PINCFG0_PULLEN));
            return;
        }
        if (mode == PinMode.InputPullDown)
        {
            Mmio.Write32(group + Saml1xPortLayout.OUTCLR_OFF, mask);
            Mmio.Write8(PinCfgAddress(pinNumber),
                (byte)(Saml1xPortLayout.PINCFG0_INEN | Saml1xPortLayout.PINCFG0_PULLEN));
            return;
        }
        Mmio.Write8(PinCfgAddress(pinNumber), (byte)Saml1xPortLayout.PINCFG0_INEN);
    }

    protected override PinMode GetPinMode(int pinNumber)
    {
        if ((Mmio.Read32(GroupBase(pinNumber) + Saml1xPortLayout.DIR_OFF) & PinMask(pinNumber)) != 0u)
        {
            return PinMode.Output;
        }
        byte config = Mmio.Read8(PinCfgAddress(pinNumber));
        if ((config & (byte)Saml1xPortLayout.PINCFG0_PULLEN) == 0)
        {
            return PinMode.Input;
        }
        uint level = Mmio.Read32(GroupBase(pinNumber) + Saml1xPortLayout.OUT_OFF);
        return (level & PinMask(pinNumber)) != 0u ? PinMode.InputPullUp : PinMode.InputPullDown;
    }

    protected override bool IsPinModeSupported(int pinNumber, PinMode mode)
    {
        return mode == PinMode.Output || mode == PinMode.Input
            || mode == PinMode.InputPullUp || mode == PinMode.InputPullDown;
    }

    protected override PinValue Read(int pinNumber)
    {
        uint levels = Mmio.Read32(GroupBase(pinNumber) + Saml1xPortLayout.IN_OFF);
        return (levels & PinMask(pinNumber)) != 0u ? PinValue.High : PinValue.Low;
    }

    protected override void Write(int pinNumber, PinValue value)
    {
        uint offset = (bool)value ? Saml1xPortLayout.OUTSET_OFF : Saml1xPortLayout.OUTCLR_OFF;
        Mmio.Write32(GroupBase(pinNumber) + offset, PinMask(pinNumber));
    }

    /// <summary>The logical pin number for a generated group base and pin index -- the form every
    /// board binding emits.</summary>
    /// <remarks>ON THE DRIVER RATHER THAN ON EACH BOARD, because which group a BASE is happens to be
    /// family truth. THIS FAMILY HAS ONE GROUP, so the method is a check rather than a switch -- and
    /// it is kept rather than dropped because every board class on every SAM family calls it, and a
    /// family that answered a pin number without validating the base would be the one place a wrong
    /// base passed silently.</remarks>
    /// <exception cref="System.ArgumentException">The base is not this family's PORT group.</exception>
    public static int LogicalPin(uint groupBase, uint pin)
    {
        if (groupBase == Saml1xInstances.PORTA_BASE) return (int)pin;
#if LAMELLA_CORLIB_LINKED
        throw new System.ArgumentException("not a PORT group base of this family");
#else
        throw new System.Exception("not a PORT group base of this family");
#endif
    }

    static uint GroupBase(int pinNumber)
    {
        return Saml1xInstances.PORTA_BASE;
    }

    static uint PinMask(int pinNumber)
    {
        return 1u << (pinNumber & 31);
    }

    static uint PinCfgAddress(int pinNumber)
    {
        return GroupBase(pinNumber) + Saml1xPortLayout.PINCFG0_OFF + (uint)(pinNumber & 31);
    }

    /// <summary>Not supported: this driver does not implement pin-change events.</summary>
    protected override void AddCallbackForPinValueChangedEvent(
        int pinNumber, PinEventTypes eventTypes, PinChangeEventHandler callback)
    {
#if LAMELLA_CORLIB_LINKED
        throw new System.NotSupportedException("SAM L10/L11 pin-change events are not implemented");
#else
        throw new System.Exception("SAM L10/L11 pin-change events are not implemented");
#endif
    }

    /// <summary>Not supported: this driver does not implement pin-change events.</summary>
    protected override void RemoveCallbackForPinValueChangedEvent(int pinNumber, PinChangeEventHandler callback)
    {
#if LAMELLA_CORLIB_LINKED
        throw new System.NotSupportedException("SAM L10/L11 pin-change events are not implemented");
#else
        throw new System.Exception("SAM L10/L11 pin-change events are not implemented");
#endif
    }
}
