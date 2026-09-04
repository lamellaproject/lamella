// A System.Device.Gpio driver for the Nordic nRF52833, P0.00..P0.31 + P1.00..P1.09. ONE driver
using System.Device.Gpio;
using Lamella.Hardware;

public sealed class Nrf52833GpioDriver : GpioDriver
{
    const uint P0_BASE = 0x50000000;
    const uint P1_BASE = 0x50000300;
    const uint OUTSET = 0x508;
    const uint OUTCLR = 0x50C;
    const uint IN = 0x510;
    const uint DIRSET = 0x518;
    const uint DIRCLR = 0x51C;
    const uint PIN_CNF = 0x700;

    const uint CNF_OUTPUT_READBACK = 0x1;
    const uint CNF_INPUT = 0x0;
    const uint CNF_INPUT_PULLDOWN = 0x4;
    const uint CNF_INPUT_PULLUP = 0xC;

    private readonly uint _p0Reserved;
    private readonly uint _p1Reserved;

    /// <summary>Reserves nothing -- every pin is the app's.</summary>
    public Nrf52833GpioDriver() : this(0u, 0u) { }

    /// <summary>Reserves the pins whose bits are set, per port, so `SetPinMode` refuses them.
    /// A board passes the lines it must keep -- its debug transport, a fixed peripheral's
    /// control pin -- because only the board knows which those are.</summary>
    public Nrf52833GpioDriver(uint port0Reserved, uint port1Reserved)
    {
        _p0Reserved = port0Reserved;
        _p1Reserved = port1Reserved;
    }

    private bool IsReserved(int pinNumber)
    {
        uint mask = 1u << (pinNumber & 31);
        return pinNumber < 32 ? (_p0Reserved & mask) != 0u : (_p1Reserved & mask) != 0u;
    }

    protected override int PinCount { get { return 42; } }

    protected override int ConvertPinNumberToLogicalNumberingScheme(int pinNumber) { return pinNumber; }

    protected override void OpenPin(int pinNumber) { }

    protected override void ClosePin(int pinNumber)
    {
        if (IsReserved(pinNumber)) return;

        Mmio.Write32(PortBase(pinNumber) + DIRCLR, PinMask(pinNumber));
        Mmio.Write32(PinCnfAddress(pinNumber), 0x2);
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
        if (mode == PinMode.Output)
        {
            Mmio.Write32(PinCnfAddress(pinNumber), CNF_OUTPUT_READBACK);
            return;
        }
        if (mode == PinMode.InputPullUp)
        {
            Mmio.Write32(PinCnfAddress(pinNumber), CNF_INPUT_PULLUP);
            return;
        }
        if (mode == PinMode.InputPullDown)
        {
            Mmio.Write32(PinCnfAddress(pinNumber), CNF_INPUT_PULLDOWN);
            return;
        }
        Mmio.Write32(PinCnfAddress(pinNumber), CNF_INPUT);
    }

    protected override PinMode GetPinMode(int pinNumber)
    {
        uint cnf = Mmio.Read32(PinCnfAddress(pinNumber));
        if ((cnf & 0x1u) != 0u) return PinMode.Output;
        uint pull = (cnf >> 2) & 0x3u;
        if (pull == 3u) return PinMode.InputPullUp;
        if (pull == 1u) return PinMode.InputPullDown;
        return PinMode.Input;
    }

    protected override bool IsPinModeSupported(int pinNumber, PinMode mode)
    {
        return mode == PinMode.Input || mode == PinMode.Output
            || mode == PinMode.InputPullUp || mode == PinMode.InputPullDown;
    }

    protected override PinValue Read(int pinNumber)
    {
        uint levels = Mmio.Read32(PortBase(pinNumber) + IN);
        return (levels & PinMask(pinNumber)) != 0u ? PinValue.High : PinValue.Low;
    }

    protected override void Write(int pinNumber, PinValue value)
    {
        uint register = (bool)value ? OUTSET : OUTCLR;
        Mmio.Write32(PortBase(pinNumber) + register, PinMask(pinNumber));
    }

    static uint PortBase(int pinNumber)
    {
        return pinNumber < 32 ? P0_BASE : P1_BASE;
    }

    static uint PinMask(int pinNumber)
    {
        return 1u << (pinNumber & 31);
    }

    static uint PinCnfAddress(int pinNumber)
    {
        return PortBase(pinNumber) + PIN_CNF + (uint)((pinNumber & 31) * 4);
    }

    /// <summary>Not supported: this driver does not implement pin-change events.</summary>
    protected override void AddCallbackForPinValueChangedEvent(
        int pinNumber, PinEventTypes eventTypes, PinChangeEventHandler callback)
    {
#if LAMELLA_CORLIB_LINKED
        throw new System.NotSupportedException("nRF52833 pin-change events are not implemented");
#else
        throw new System.Exception("nRF52833 pin-change events are not implemented");
#endif
    }

    /// <summary>Not supported: this driver does not implement pin-change events.</summary>
    protected override void RemoveCallbackForPinValueChangedEvent(int pinNumber, PinChangeEventHandler callback)
    {
#if LAMELLA_CORLIB_LINKED
        throw new System.NotSupportedException("nRF52833 pin-change events are not implemented");
#else
        throw new System.Exception("nRF52833 pin-change events are not implemented");
#endif
    }
}
