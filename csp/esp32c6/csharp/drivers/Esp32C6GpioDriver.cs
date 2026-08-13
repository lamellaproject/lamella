// A System.Device.Gpio driver for the Espressif ESP32-C6 (RISC-V RV32IMAC), GPIO0..GPIO30.
using System.Device.Gpio;
using Lamella.Hardware;

public sealed class Esp32C6GpioDriver : GpioDriver
{
    const uint GPIO_OUT_W1TS = 0x60091008;
    const uint GPIO_OUT_W1TC = 0x6009100C;
    const uint GPIO_ENABLE = 0x60091020;
    const uint GPIO_ENABLE_W1TS = 0x60091024;
    const uint GPIO_ENABLE_W1TC = 0x60091028;
    const uint GPIO_IN = 0x6009103C;

    const uint GPIO_FUNC0_OUT_SEL_CFG = 0x60091554;
    const uint SIG_GPIO_OUT = 128;

    const uint IO_MUX_GPIO0 = 0x60090004;
    const uint FUN_WPD = 0x80;
    const uint FUN_WPU = 0x100;
    const uint FUN_IE = 0x200;
    const uint FUN_DRV_DEFAULT = 0x800;
    const uint MCU_SEL_GPIO = 0x1000;

    const int UsbDMinus = 12;
    const int UsbDPlus = 13;

    protected override int PinCount { get { return 31; } }

    protected override int ConvertPinNumberToLogicalNumberingScheme(int pinNumber) { return pinNumber; }

    protected override void OpenPin(int pinNumber) { }

    protected override void ClosePin(int pinNumber)
    {
        Mmio.Write32(GPIO_ENABLE_W1TC, 1u << pinNumber);
        Mmio.Write32(GPIO_FUNC0_OUT_SEL_CFG + (uint)(pinNumber * 4), SIG_GPIO_OUT);
    }

    protected override void SetPinMode(int pinNumber, PinMode mode)
    {
        if (pinNumber == UsbDMinus || pinNumber == UsbDPlus)
        {
#if LAMELLA_CORLIB_LINKED
            throw new System.ArgumentException("GPIO12/GPIO13 carry the USB-Serial-JTAG wire");
#else
            throw new System.Exception("GPIO12/GPIO13 carry the USB-Serial-JTAG wire");
#endif
        }
        uint mux = MCU_SEL_GPIO | FUN_DRV_DEFAULT | FUN_IE;
        uint mask = 1u << pinNumber;
        if (mode == PinMode.Output)
        {
            Mmio.Write32(IO_MUX_GPIO0 + (uint)(pinNumber * 4), mux);
            Mmio.Write32(GPIO_FUNC0_OUT_SEL_CFG + (uint)(pinNumber * 4), SIG_GPIO_OUT);
            Mmio.Write32(GPIO_ENABLE_W1TS, mask);
        }
        else
        {
            if (mode == PinMode.InputPullUp) mux |= FUN_WPU;
            else if (mode == PinMode.InputPullDown) mux |= FUN_WPD;
            Mmio.Write32(IO_MUX_GPIO0 + (uint)(pinNumber * 4), mux);
            Mmio.Write32(GPIO_ENABLE_W1TC, mask);
        }
    }

    protected override PinMode GetPinMode(int pinNumber)
    {
        return ((Mmio.Read32(GPIO_ENABLE) >> pinNumber) & 1u) != 0u ? PinMode.Output : PinMode.Input;
    }

    protected override bool IsPinModeSupported(int pinNumber, PinMode mode)
    {
        return pinNumber != UsbDMinus && pinNumber != UsbDPlus;
    }

    protected override PinValue Read(int pinNumber)
    {
        return (int)((Mmio.Read32(GPIO_IN) >> pinNumber) & 1u);
    }

    protected override void Write(int pinNumber, PinValue value)
    {
        if ((bool)value) Mmio.Write32(GPIO_OUT_W1TS, 1u << pinNumber);
        else Mmio.Write32(GPIO_OUT_W1TC, 1u << pinNumber);
    }


    /// <summary>Not supported: this driver does not implement pin-change events.</summary>
    protected override void AddCallbackForPinValueChangedEvent(
        int pinNumber, PinEventTypes eventTypes, PinChangeEventHandler callback)
    {
#if LAMELLA_CORLIB_LINKED
        throw new System.NotSupportedException("ESP32-C6 pin-change events are not implemented");
#else
        throw new System.Exception("ESP32-C6 pin-change events are not implemented");
#endif
    }

    /// <summary>Not supported: this driver does not implement pin-change events.</summary>
    protected override void RemoveCallbackForPinValueChangedEvent(int pinNumber, PinChangeEventHandler callback)
    {
#if LAMELLA_CORLIB_LINKED
        throw new System.NotSupportedException("ESP32-C6 pin-change events are not implemented");
#else
        throw new System.Exception("ESP32-C6 pin-change events are not implemented");
#endif
    }
}
