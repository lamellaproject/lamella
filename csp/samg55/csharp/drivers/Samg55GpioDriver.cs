// A System.Device.Gpio driver for the Microchip SAM G55, PA0..PA31 + PB0..PB15.
using System.Device.Gpio;
using Lamella.Generated;
using Lamella.Hardware;

public sealed class Samg55GpioDriver : GpioDriver
{
    protected override int PinCount { get { return 48; } }

    protected override int ConvertPinNumberToLogicalNumberingScheme(int pinNumber) { return pinNumber; }

    protected override void OpenPin(int pinNumber) { }

    protected override void ClosePin(int pinNumber)
    {
        uint c = ControllerBase(pinNumber);
        uint mask = LineMask(pinNumber);
        Mmio.Write32(c + Samg55PioLayout.ODR_OFF, mask);
        Mmio.Write32(c + Samg55PioLayout.PUDR_OFF, mask);
        Mmio.Write32(c + Samg55PioLayout.PPDDR_OFF, mask);
    }

    protected override void SetPinMode(int pinNumber, PinMode mode)
    {
        uint c = ControllerBase(pinNumber);
        uint mask = LineMask(pinNumber);

        Mmio.Write32(c + Samg55PioLayout.PER_OFF, mask);

        if (mode == PinMode.Output)
        {
            Mmio.Write32(c + Samg55PioLayout.PUDR_OFF, mask);
            Mmio.Write32(c + Samg55PioLayout.PPDDR_OFF, mask);
            Mmio.Write32(c + Samg55PioLayout.OER_OFF, mask);
            return;
        }

        Mmio.Write32(c + Samg55PioLayout.ODR_OFF, mask);
        if (mode == PinMode.InputPullUp)
        {
            Mmio.Write32(c + Samg55PioLayout.PPDDR_OFF, mask);
            Mmio.Write32(c + Samg55PioLayout.PUER_OFF, mask);
            return;
        }
        if (mode == PinMode.InputPullDown)
        {
            Mmio.Write32(c + Samg55PioLayout.PUDR_OFF, mask);
            Mmio.Write32(c + Samg55PioLayout.PPDER_OFF, mask);
            return;
        }
        Mmio.Write32(c + Samg55PioLayout.PUDR_OFF, mask);
        Mmio.Write32(c + Samg55PioLayout.PPDDR_OFF, mask);
    }

    protected override PinMode GetPinMode(int pinNumber)
    {
        uint c = ControllerBase(pinNumber);
        uint mask = LineMask(pinNumber);
        if ((Mmio.Read32(c + Samg55PioLayout.OSR_OFF) & mask) != 0u)
        {
            return PinMode.Output;
        }
        if ((Mmio.Read32(c + Samg55PioLayout.PUSR_OFF) & mask) == 0u)
        {
            return PinMode.InputPullUp;
        }
        if ((Mmio.Read32(c + Samg55PioLayout.PPDSR_OFF) & mask) == 0u)
        {
            return PinMode.InputPullDown;
        }
        return PinMode.Input;
    }

    protected override bool IsPinModeSupported(int pinNumber, PinMode mode)
    {
        return mode == PinMode.Output || mode == PinMode.Input
            || mode == PinMode.InputPullUp || mode == PinMode.InputPullDown;
    }

    protected override PinValue Read(int pinNumber)
    {
        uint levels = Mmio.Read32(ControllerBase(pinNumber) + Samg55PioLayout.PDSR_OFF);
        return (levels & LineMask(pinNumber)) != 0u ? PinValue.High : PinValue.Low;
    }

    protected override void Write(int pinNumber, PinValue value)
    {
        uint offset = (bool)value ? Samg55PioLayout.SODR_OFF : Samg55PioLayout.CODR_OFF;
        Mmio.Write32(ControllerBase(pinNumber) + offset, LineMask(pinNumber));
    }

    /// <summary>The logical pin number for a generated controller base and line index -- the form
    /// every board binding emits.</summary>
    /// <remarks>ON THE DRIVER RATHER THAN ON EACH BOARD, because which controller a BASE is happens
    /// to be family truth: the bases come from this family's instance map and the lines-per-
    /// controller is this family's, so each board class carrying its own switch would be one copy
    /// per board of one fact.</remarks>
    /// <exception cref="System.ArgumentException">The base is not a PIO controller of this family.</exception>
    public static int LogicalPin(uint controllerBase, uint line)
    {
        if (controllerBase == Samg55Instances.PIOA_BASE) return (int)line;
        if (controllerBase == Samg55Instances.PIOB_BASE) return 32 + (int)line;
#if LAMELLA_CORLIB_LINKED
        throw new System.ArgumentException("not a PIO controller base of this family");
#else
        throw new System.Exception("not a PIO controller base of this family");
#endif
    }

    static uint ControllerBase(int pinNumber)
    {
        return pinNumber < 32 ? Samg55Instances.PIOA_BASE : Samg55Instances.PIOB_BASE;
    }

    static uint LineMask(int pinNumber)
    {
        return 1u << (pinNumber & 31);
    }

    /// <summary>Not supported: this driver does not implement pin-change events.</summary>
    protected override void AddCallbackForPinValueChangedEvent(
        int pinNumber, PinEventTypes eventTypes, PinChangeEventHandler callback)
    {
#if LAMELLA_CORLIB_LINKED
        throw new System.NotSupportedException("SAM G55 pin-change events are not implemented");
#else
        throw new System.Exception("SAM G55 pin-change events are not implemented");
#endif
    }

    /// <summary>Not supported: this driver does not implement pin-change events.</summary>
    protected override void RemoveCallbackForPinValueChangedEvent(int pinNumber, PinChangeEventHandler callback)
    {
#if LAMELLA_CORLIB_LINKED
        throw new System.NotSupportedException("SAM G55 pin-change events are not implemented");
#else
        throw new System.Exception("SAM G55 pin-change events are not implemented");
#endif
    }
}
