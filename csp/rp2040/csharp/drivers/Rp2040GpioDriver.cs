// A System.Device.Gpio driver for the RP2040 user-bank GPIOs (GP0..GP29), over
using System.Device.Gpio;
using Lamella.Generated;
using Lamella.Hardware;

public sealed class Rp2040GpioDriver : GpioDriver
{
    private readonly uint _resetsClr;
    private readonly uint _resetsDone;
    private readonly uint _ioCtrl0;
    private readonly uint _pads0;
    private readonly uint _sioIn;
    private readonly uint _sioOutSet;
    private readonly uint _sioOutClr;
    private readonly uint _sioOutXor;
    private readonly uint _sioOe;
    private readonly uint _sioOeSet;
    private readonly uint _sioOeClr;
    private readonly uint _bankResetMask;

    private readonly uint _reserved;

    /// <summary>Reserves nothing -- every pin is the app's.</summary>
    public Rp2040GpioDriver() : this(0u) { }

    /// <summary>Reserves the pins whose bits are set, so <c>SetPinMode</c> refuses them. A board
    /// passes the lines it must keep -- its debug transport, a fixed peripheral's control pin --
    /// because only the board knows which those are.</summary>
    /// <remarks>THE PICO W IS WHY THIS EXISTS ON THIS FAMILY. Its wireless part sits on GP23, GP24,
    /// GP25 and GP29, and GP25 is the pin that drives the user LED on an RP2040 product carrying no
    /// radio. So the line that blinks a Pico asserts the radio's chip select on a Pico W, from
    /// identical source and with nothing to see.</remarks>
    public Rp2040GpioDriver(uint reserved)
    {
        _reserved = reserved;
        _resetsClr = Rp2040Instances.RESETS_CLR_BASE + Rp2040ResetsLayout.RESET_OFF;
        _resetsDone = Rp2040Instances.RESETS_BASE + Rp2040ResetsLayout.RESET_DONE_OFF;
        _ioCtrl0 = Rp2040Instances.IO_BANK0_BASE + Rp2040IoBank0Layout.GPIO0_CTRL_OFF;
        _pads0 = Rp2040Instances.PADS_BANK0_BASE + Rp2040PadsBank0Layout.GPIO0_OFF;
        _sioIn = Rp2040Instances.SIO_BASE + Rp2040SioLayout.GPIO_IN_OFF;
        _sioOutSet = Rp2040Instances.SIO_BASE + Rp2040SioLayout.GPIO_OUT_SET_OFF;
        _sioOutClr = Rp2040Instances.SIO_BASE + Rp2040SioLayout.GPIO_OUT_CLR_OFF;
        _sioOutXor = Rp2040Instances.SIO_BASE + Rp2040SioLayout.GPIO_OUT_XOR_OFF;
        _sioOe = Rp2040Instances.SIO_BASE + Rp2040SioLayout.GPIO_OE_OFF;
        _sioOeSet = Rp2040Instances.SIO_BASE + Rp2040SioLayout.GPIO_OE_SET_OFF;
        _sioOeClr = Rp2040Instances.SIO_BASE + Rp2040SioLayout.GPIO_OE_CLR_OFF;
        _bankResetMask = Rp2040Instances.IO_BANK0_RESET_MASK | Rp2040Instances.PADS_BANK0_RESET_MASK;
    }

    protected override int PinCount { get { return 30; } }

    protected override int ConvertPinNumberToLogicalNumberingScheme(int pinNumber) { return pinNumber; }

    protected override void OpenPin(int pinNumber) { }

    protected override void ClosePin(int pinNumber)
    {
        if (IsReserved(pinNumber)) return;

        Mmio.Write32(_sioOeClr, 1u << pinNumber);
    }

    void EnsureBankReady()
    {
        Mmio.Write32(_resetsClr, _bankResetMask);
        for (int spin = 0; spin < 100000; spin++)
        {
            if ((Mmio.Read32(_resetsDone) & _bankResetMask) == _bankResetMask) return;
        }
    }

    private bool IsReserved(int pinNumber)
    {
        return (_reserved & (1u << pinNumber)) != 0u;
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
        EnsureBankReady();
        Mmio.Write32(_ioCtrl0 + Rp2040IoBank0Layout.GPIO_CTRL_STRIDE * (uint)pinNumber,
            Rp2040IoBank0Layout.FUNCSEL_SIO);
        uint mask = 1u << pinNumber;
        uint padAddress = _pads0 + Rp2040PadsBank0Layout.GPIO_STRIDE * (uint)pinNumber;

        uint pad = Mmio.Read32(padAddress);
        pad |= Rp2040PadsBank0Layout.GPIO0_IE;
        pad &= ~Rp2040PadsBank0Layout.GPIO0_OD;
        pad &= ~(Rp2040PadsBank0Layout.GPIO0_PUE | Rp2040PadsBank0Layout.GPIO0_PDE);
        if (mode == PinMode.InputPullUp) pad |= Rp2040PadsBank0Layout.GPIO0_PUE;
        else if (mode == PinMode.InputPullDown) pad |= Rp2040PadsBank0Layout.GPIO0_PDE;
        Mmio.Write32(padAddress, pad);

        if (mode == PinMode.Output) Mmio.Write32(_sioOeSet, mask);
        else Mmio.Write32(_sioOeClr, mask);
    }

    protected override PinMode GetPinMode(int pinNumber)
    {
        return ((Mmio.Read32(_sioOe) >> pinNumber) & 1u) != 0u ? PinMode.Output : PinMode.Input;
    }

    protected override bool IsPinModeSupported(int pinNumber, PinMode mode) { return true; }

    protected override PinValue Read(int pinNumber)
    {
        return (int)((Mmio.Read32(_sioIn) >> pinNumber) & 1u);
    }

    protected override void Write(int pinNumber, PinValue value)
    {
        if ((bool)value) Mmio.Write32(_sioOutSet, 1u << pinNumber);
        else Mmio.Write32(_sioOutClr, 1u << pinNumber);
    }

    protected override void Toggle(int pinNumber)
    {
        Mmio.Write32(_sioOutXor, 1u << pinNumber);
    }

    /// <summary>Not supported: this driver does not implement pin-change events.</summary>
    protected override void AddCallbackForPinValueChangedEvent(
        int pinNumber, PinEventTypes eventTypes, PinChangeEventHandler callback)
    {
#if LAMELLA_CORLIB_LINKED
        throw new System.NotSupportedException("RP2040 pin-change events are not implemented");
#else
        throw new System.Exception("RP2040 pin-change events are not implemented");
#endif
    }

    /// <summary>Not supported: this driver does not implement pin-change events.</summary>
    protected override void RemoveCallbackForPinValueChangedEvent(int pinNumber, PinChangeEventHandler callback)
    {
#if LAMELLA_CORLIB_LINKED
        throw new System.NotSupportedException("RP2040 pin-change events are not implemented");
#else
        throw new System.Exception("RP2040 pin-change events are not implemented");
#endif
    }
}
