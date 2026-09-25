// A System.Device.Gpio driver for the Nordic nRF51 series: the one GPIO port, P0.00 to P0.31, as
// logical pins 0 to 31.
//
// Which of those pins a part bonds out depends on its package, so the driver accepts all thirty-two
// and a board names only the pins its part has. Pins a board must keep for itself -- a debug
// transport, the control line of a soldered-on part -- are passed to the constructor, and the driver
// refuses to reconfigure them.
//
// An output keeps its input buffer connected, so Read returns the level on the pad rather than the
// level last written: a pin held against its drive reads as what it is.
using System.Device.Gpio;
using Lamella.Generated;
using Lamella.Hardware;

public sealed class Nrf51GpioDriver : GpioDriver
{
    private readonly uint _reserved;

    /// <summary>Creates the driver with no pin reserved: every pin is the application's.</summary>
    public Nrf51GpioDriver() : this(0u) { }

    /// <summary>Creates the driver with the pins whose bits are set in <paramref name="reserved"/>
    /// kept for the board, so <c>SetPinMode</c> refuses them and <c>ClosePin</c> leaves them as they
    /// are.</summary>
    /// <param name="reserved">One bit per pin: bit n set reserves P0.n. A board passes the lines it
    /// must keep, such as its debug transport, because only the board knows which those are.</param>
    public Nrf51GpioDriver(uint reserved)
    {
        _reserved = reserved;
    }

    protected override int PinCount { get { return 32; } }

    protected override int ConvertPinNumberToLogicalNumberingScheme(int pinNumber) { return pinNumber; }

    protected override void OpenPin(int pinNumber) { }

    protected override void ClosePin(int pinNumber)
    {
        // A reserved pin is left exactly as it is. SetPinMode refuses one, but a pin can be opened
        // and closed without a mode ever being set, and disposing a GpioController closes every pin
        // it opened. It returns rather than throwing, because a Dispose that throws loses whatever
        // else was being disposed.
        if (IsReserved(pinNumber)) return;

        // Stop driving, then return the pin to its reset configuration: an input with its buffer
        // disconnected and no pull.
        Mmio.Write32(Nrf51Instances.PORT0_BASE + Nrf51GpioLayout.DIRCLR_OFF, PinMask(pinNumber));
        Mmio.Write32(PinCnfAddress(pinNumber),
            Nrf51GpioLayout.INPUT_DISCONNECT << (int)Nrf51GpioLayout.PIN_CNF0_INPUT_LSB);
    }

    /// <exception cref="System.ArgumentException">The pin is reserved by the board.</exception>
    protected override void SetPinMode(int pinNumber, PinMode mode)
    {
        if (IsReserved(pinNumber))
        {
            BadArgument("pin is reserved by the board");
        }
        uint direction = Nrf51GpioLayout.DIR_INPUT;
        uint pull = Nrf51GpioLayout.PULL_DISABLED;
        if (mode == PinMode.Output)
        {
            direction = Nrf51GpioLayout.DIR_OUTPUT;
        }
        else if (mode == PinMode.InputPullUp)
        {
            pull = Nrf51GpioLayout.PULL_PULLUP;
        }
        else if (mode == PinMode.InputPullDown)
        {
            pull = Nrf51GpioLayout.PULL_PULLDOWN;
        }
        // One write sets the whole configuration. The input buffer stays connected in every mode,
        // including output, so Read sees the pad; the drive is standard and level sensing is off.
        uint config = (direction << (int)Nrf51GpioLayout.PIN_CNF0_DIR_LSB)
            | (Nrf51GpioLayout.INPUT_CONNECT << (int)Nrf51GpioLayout.PIN_CNF0_INPUT_LSB)
            | (pull << (int)Nrf51GpioLayout.PIN_CNF0_PULL_LSB)
            | (Nrf51GpioLayout.DRIVE_S0S1 << (int)Nrf51GpioLayout.PIN_CNF0_DRIVE_LSB)
            | (Nrf51GpioLayout.SENSE_DISABLED << (int)Nrf51GpioLayout.PIN_CNF0_SENSE_LSB);
        Mmio.Write32(PinCnfAddress(pinNumber), config);
    }

    protected override PinMode GetPinMode(int pinNumber)
    {
        // Read from the hardware rather than from a record of what was set, so a pin configured by
        // other code is reported as it stands.
        uint config = Mmio.Read32(PinCnfAddress(pinNumber));
        if ((config & Nrf51GpioLayout.PIN_CNF0_DIR) != 0u)
        {
            return PinMode.Output;
        }
        uint pull = (config & Nrf51GpioLayout.PIN_CNF0_PULL) >> (int)Nrf51GpioLayout.PIN_CNF0_PULL_LSB;
        if (pull == Nrf51GpioLayout.PULL_PULLUP) return PinMode.InputPullUp;
        if (pull == Nrf51GpioLayout.PULL_PULLDOWN) return PinMode.InputPullDown;
        return PinMode.Input;
    }

    protected override bool IsPinModeSupported(int pinNumber, PinMode mode)
    {
        return mode == PinMode.Input || mode == PinMode.Output
            || mode == PinMode.InputPullUp || mode == PinMode.InputPullDown;
    }

    protected override PinValue Read(int pinNumber)
    {
        uint levels = Mmio.Read32(Nrf51Instances.PORT0_BASE + Nrf51GpioLayout.IN_OFF);
        return (levels & PinMask(pinNumber)) != 0u ? PinValue.High : PinValue.Low;
    }

    protected override void Write(int pinNumber, PinValue value)
    {
        uint offset = (bool)value ? Nrf51GpioLayout.OUTSET_OFF : Nrf51GpioLayout.OUTCLR_OFF;
        Mmio.Write32(Nrf51Instances.PORT0_BASE + offset, PinMask(pinNumber));
    }

    /// <summary>Not supported: this driver does not implement pin-change events.</summary>
    /// <exception cref="System.NotSupportedException">Always.</exception>
    protected override void AddCallbackForPinValueChangedEvent(
        int pinNumber, PinEventTypes eventTypes, PinChangeEventHandler callback)
    {
        Unsupported("nRF51 pin-change events are not implemented");
    }

    /// <summary>Not supported: this driver does not implement pin-change events.</summary>
    /// <exception cref="System.NotSupportedException">Always.</exception>
    protected override void RemoveCallbackForPinValueChangedEvent(int pinNumber, PinChangeEventHandler callback)
    {
        Unsupported("nRF51 pin-change events are not implemented");
    }

    bool IsReserved(int pinNumber)
    {
        return (_reserved & PinMask(pinNumber)) != 0u;
    }

    static uint PinMask(int pinNumber)
    {
        return 1u << (pinNumber & 31);
    }

    static uint PinCnfAddress(int pinNumber)
    {
        return Nrf51Instances.PORT0_BASE + Nrf51GpioLayout.PIN_CNF0_OFF
            + (uint)(pinNumber & 31) * Nrf51GpioLayout.PIN_CNF_STRIDE;
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
}
