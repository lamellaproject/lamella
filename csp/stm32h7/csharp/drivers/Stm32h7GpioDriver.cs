// A System.Device.Gpio driver for the STM32H7's eleven GPIO ports, PA0 through PK15, over
using System.Device.Gpio;
using Lamella.Generated;
using Lamella.Hardware;

public sealed class Stm32h7GpioDriver : GpioDriver
{
    private const int PinsPerPort = 16;
    private const int PortCount = 11;

    private readonly uint _reservedLow;
    private readonly uint _reservedHigh;

    /// <summary>Reserves nothing -- every pin is the app's.</summary>
    public Stm32h7GpioDriver() : this(0u, 0u) { }

    /// <summary>Reserves the pins whose bits are set, so <c>SetPinMode</c> refuses them. A board
    /// passes the lines it must keep -- its debug transport, a fixed peripheral's control pin --
    /// because only the board knows which those are.</summary>
    /// <remarks>TWO MASKS RATHER THAN ONE, AND THEY COVER PA0 THROUGH PD15 ONLY. PA13 and PA14 are
    /// SWDIO and SWCLK and come out of reset in alternate function, so the pins a board most needs
    /// to protect are the low ones; a board wanting to reserve a pin above PD15 is asking for
    /// something this shape cannot express and should say so rather than be given a silent
    /// half-answer.</remarks>
    public Stm32h7GpioDriver(uint reservedLow, uint reservedHigh)
    {
        _reservedLow = reservedLow;
        _reservedHigh = reservedHigh;
    }

    protected override int PinCount { get { return PortCount * PinsPerPort; } }

    protected override int ConvertPinNumberToLogicalNumberingScheme(int pinNumber) { return pinNumber; }

    protected override void OpenPin(int pinNumber) { }

    protected override void ClosePin(int pinNumber)
    {
        if (IsReserved(pinNumber)) return;

        WriteMode(pinNumber, Stm32h7GpioLayout.MODER_MODE_ANALOG);
        WritePull(pinNumber, PullNone);
    }

    private static readonly uint PullNone = Stm32h7GpioLayout.PUPDR_NONE;
    private static readonly uint PullUp = Stm32h7GpioLayout.PUPDR_PULL_UP;
    private static readonly uint PullDown = Stm32h7GpioLayout.PUPDR_PULL_DOWN;

    private bool IsReserved(int pinNumber)
    {
        if (pinNumber < 32) return (_reservedLow & (1u << pinNumber)) != 0u;
        if (pinNumber < 64) return (_reservedHigh & (1u << (pinNumber - 32))) != 0u;
        return false;
    }

    /// <summary>The logical pin number for a generated port base and pin index -- the form every
    /// board binding emits.</summary>
    /// <remarks>ON THE DRIVER RATHER THAN ON EACH BOARD, because which port a BASE is happens to be
    /// family truth: the bases come from this family's instance map and the ports-per-word is this
    /// family's, so four board classes each carrying an eleven-way switch would be four copies of
    /// one fact. What stays a BOARD fact is which base a device sits on, and that is what a board
    /// passes in.</remarks>
    /// <exception cref="System.ArgumentException">The base is not a GPIO port of this family.</exception>
    public static int LogicalPin(uint portBase, uint pin)
    {
        for (int port = 0; port < PortCount; port++)
        {
            if (PortBase(port) == portBase) return port * PinsPerPort + (int)pin;
        }
#if LAMELLA_CORLIB_LINKED
        throw new System.ArgumentException("not a GPIO port base of this family");
#else
        throw new System.Exception("not a GPIO port base of this family");
#endif
    }

    private static int PortOf(int pinNumber) { return pinNumber / PinsPerPort; }

    private static int IndexOf(int pinNumber) { return pinNumber % PinsPerPort; }

    private static uint PortBase(int port)
    {
        switch (port)
        {
            case 0: return Stm32h7Instances.GPIOA_BASE;
            case 1: return Stm32h7Instances.GPIOB_BASE;
            case 2: return Stm32h7Instances.GPIOC_BASE;
            case 3: return Stm32h7Instances.GPIOD_BASE;
            case 4: return Stm32h7Instances.GPIOE_BASE;
            case 5: return Stm32h7Instances.GPIOF_BASE;
            case 6: return Stm32h7Instances.GPIOG_BASE;
            case 7: return Stm32h7Instances.GPIOH_BASE;
            case 8: return Stm32h7Instances.GPIOI_BASE;
            case 9: return Stm32h7Instances.GPIOJ_BASE;
            default: return Stm32h7Instances.GPIOK_BASE;
        }
    }

    private static uint PortClockMask(int port)
    {
        switch (port)
        {
            case 0: return Stm32h7Instances.GPIOA_RCC_EN_MASK;
            case 1: return Stm32h7Instances.GPIOB_RCC_EN_MASK;
            case 2: return Stm32h7Instances.GPIOC_RCC_EN_MASK;
            case 3: return Stm32h7Instances.GPIOD_RCC_EN_MASK;
            case 4: return Stm32h7Instances.GPIOE_RCC_EN_MASK;
            case 5: return Stm32h7Instances.GPIOF_RCC_EN_MASK;
            case 6: return Stm32h7Instances.GPIOG_RCC_EN_MASK;
            case 7: return Stm32h7Instances.GPIOH_RCC_EN_MASK;
            case 8: return Stm32h7Instances.GPIOI_RCC_EN_MASK;
            case 9: return Stm32h7Instances.GPIOJ_RCC_EN_MASK;
            default: return Stm32h7Instances.GPIOK_RCC_EN_MASK;
        }
    }

    private static void EnsurePortClocked(int port)
    {
        uint enableRegister = Stm32h7Instances.RCC_BASE + Stm32h7Instances.GPIOA_RCC_EN_OFF;
        uint mask = PortClockMask(port);
        uint current = Mmio.Read32(enableRegister);
        if ((current & mask) == mask) return;
        Mmio.Write32(enableRegister, current | mask);
        Mmio.Read32(enableRegister);
    }

    private static void WriteTwoBitField(uint register, int index, uint value)
    {
        uint current = Mmio.Read32(register);
        int shift = index * 2;
        uint cleared = current & ~(3u << shift);
        Mmio.Write32(register, cleared | (value << shift));
    }

    private static void WriteMode(int pinNumber, uint mode)
    {
        int port = PortOf(pinNumber);
        EnsurePortClocked(port);
        WriteTwoBitField(PortBase(port) + Stm32h7GpioLayout.MODER_OFF, IndexOf(pinNumber), mode);
    }

    private static void WritePull(int pinNumber, uint pull)
    {
        int port = PortOf(pinNumber);
        WriteTwoBitField(PortBase(port) + Stm32h7GpioLayout.PUPDR_OFF, IndexOf(pinNumber), pull);
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
        if (mode == PinMode.InputPullUp) WritePull(pinNumber, PullUp);
        else if (mode == PinMode.InputPullDown) WritePull(pinNumber, PullDown);
        else WritePull(pinNumber, PullNone);

        WriteMode(pinNumber,
            mode == PinMode.Output ? Stm32h7GpioLayout.MODER_MODE_OUTPUT : Stm32h7GpioLayout.MODER_MODE_INPUT);
    }

    protected override PinMode GetPinMode(int pinNumber)
    {
        int port = PortOf(pinNumber);
        uint moder = Mmio.Read32(PortBase(port) + Stm32h7GpioLayout.MODER_OFF);
        uint mode = (moder >> (IndexOf(pinNumber) * 2)) & 3u;
        if (mode == Stm32h7GpioLayout.MODER_MODE_OUTPUT) return PinMode.Output;
        uint pupdr = Mmio.Read32(PortBase(port) + Stm32h7GpioLayout.PUPDR_OFF);
        uint pull = (pupdr >> (IndexOf(pinNumber) * 2)) & 3u;
        if (pull == PullUp) return PinMode.InputPullUp;
        if (pull == PullDown) return PinMode.InputPullDown;
        return PinMode.Input;
    }

    protected override bool IsPinModeSupported(int pinNumber, PinMode mode) { return true; }

    protected override PinValue Read(int pinNumber)
    {
        int port = PortOf(pinNumber);
        uint idr = Mmio.Read32(PortBase(port) + Stm32h7GpioLayout.IDR_OFF);
        return (int)((idr >> IndexOf(pinNumber)) & 1u);
    }

    protected override void Write(int pinNumber, PinValue value)
    {
        int port = PortOf(pinNumber);
        int index = IndexOf(pinNumber);
        uint bit = (bool)value ? 1u << index : 1u << (index + 16);
        Mmio.Write32(PortBase(port) + Stm32h7GpioLayout.BSRR_OFF, bit);
    }


    /// <summary>Not supported: this driver does not implement pin-change events.</summary>
    protected override void AddCallbackForPinValueChangedEvent(
        int pinNumber, PinEventTypes eventTypes, PinChangeEventHandler callback)
    {
#if LAMELLA_CORLIB_LINKED
        throw new System.NotSupportedException("STM32H7 pin-change events are not implemented");
#else
        throw new System.Exception("STM32H7 pin-change events are not implemented");
#endif
    }

    /// <summary>Not supported: this driver does not implement pin-change events.</summary>
    protected override void RemoveCallbackForPinValueChangedEvent(int pinNumber, PinChangeEventHandler callback)
    {
#if LAMELLA_CORLIB_LINKED
        throw new System.NotSupportedException("STM32H7 pin-change events are not implemented");
#else
        throw new System.Exception("STM32H7 pin-change events are not implemented");
#endif
    }
}
