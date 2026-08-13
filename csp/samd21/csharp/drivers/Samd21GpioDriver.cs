// A System.Device.Gpio driver for the Microchip SAMD21, PA00..PA31 + PB00..PB31. Subclasses the
using System.Device.Gpio;
using Lamella.Generated;
using Lamella.Hardware;

public sealed class Samd21GpioDriver : GpioDriver
{
    protected override int PinCount { get { return 64; } }

    protected override int ConvertPinNumberToLogicalNumberingScheme(int pinNumber) { return pinNumber; }

    protected override void OpenPin(int pinNumber) { }

    protected override void ClosePin(int pinNumber)
    {
        Mmio.Write32(GroupBase(pinNumber) + Samd21PortLayout.DIRCLR_OFF, PinMask(pinNumber));
        Mmio.Write8(PinCfgAddress(pinNumber), 0);
    }

    protected override void SetPinMode(int pinNumber, PinMode mode)
    {
        uint group = GroupBase(pinNumber);
        uint mask = PinMask(pinNumber);
        if (mode == PinMode.Output)
        {
            Mmio.Write8(PinCfgAddress(pinNumber), (byte)Samd21PortLayout.PINCFG0_INEN);
            Mmio.Write32(group + Samd21PortLayout.DIRSET_OFF, mask);
            return;
        }

        Mmio.Write32(group + Samd21PortLayout.DIRCLR_OFF, mask);
        if (mode == PinMode.InputPullUp)
        {
            Mmio.Write32(group + Samd21PortLayout.OUTSET_OFF, mask);
            Mmio.Write8(PinCfgAddress(pinNumber),
                (byte)(Samd21PortLayout.PINCFG0_INEN | Samd21PortLayout.PINCFG0_PULLEN));
            return;
        }
        if (mode == PinMode.InputPullDown)
        {
            Mmio.Write32(group + Samd21PortLayout.OUTCLR_OFF, mask);
            Mmio.Write8(PinCfgAddress(pinNumber),
                (byte)(Samd21PortLayout.PINCFG0_INEN | Samd21PortLayout.PINCFG0_PULLEN));
            return;
        }
        Mmio.Write8(PinCfgAddress(pinNumber), (byte)Samd21PortLayout.PINCFG0_INEN);
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

    /// <summary>Not supported: this driver does not implement pin-change events.</summary>
    protected override void AddCallbackForPinValueChangedEvent(
        int pinNumber, PinEventTypes eventTypes, PinChangeEventHandler callback)
    {
#if LAMELLA_CORLIB_LINKED
        throw new System.NotSupportedException("SAMD21 pin-change events are not implemented");
#else
        throw new System.Exception("SAMD21 pin-change events are not implemented");
#endif
    }

    /// <summary>Not supported: this driver does not implement pin-change events.</summary>
    protected override void RemoveCallbackForPinValueChangedEvent(int pinNumber, PinChangeEventHandler callback)
    {
#if LAMELLA_CORLIB_LINKED
        throw new System.NotSupportedException("SAMD21 pin-change events are not implemented");
#else
        throw new System.Exception("SAMD21 pin-change events are not implemented");
#endif
    }
}
