// A System.Device.Gpio driver for the RP2350 bank-0 GPIOs (GP0..GP31), over
using System.Device.Gpio;
using Lamella.Generated;
using Lamella.Hardware;

public sealed class Rp2350GpioDriver : GpioDriver
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

    public Rp2350GpioDriver()
    {
        _resetsClr = Rp2350Instances.RESETS_CLR_BASE + Rp2350ResetsLayout.RESET_OFF;
        _resetsDone = Rp2350Instances.RESETS_BASE + Rp2350ResetsLayout.RESET_DONE_OFF;
        _ioCtrl0 = Rp2350Instances.IO_BANK0_BASE + Rp2350IoBank0Layout.GPIO0_CTRL_OFF;
        _pads0 = Rp2350Instances.PADS_BANK0_BASE + Rp2350PadsBank0Layout.GPIO0_OFF;
        _sioIn = Rp2350Instances.SIO_BASE + Rp2350SioLayout.GPIO_IN_OFF;
        _sioOutSet = Rp2350Instances.SIO_BASE + Rp2350SioLayout.GPIO_OUT_SET_OFF;
        _sioOutClr = Rp2350Instances.SIO_BASE + Rp2350SioLayout.GPIO_OUT_CLR_OFF;
        _sioOutXor = Rp2350Instances.SIO_BASE + Rp2350SioLayout.GPIO_OUT_XOR_OFF;
        _sioOe = Rp2350Instances.SIO_BASE + Rp2350SioLayout.GPIO_OE_OFF;
        _sioOeSet = Rp2350Instances.SIO_BASE + Rp2350SioLayout.GPIO_OE_SET_OFF;
        _sioOeClr = Rp2350Instances.SIO_BASE + Rp2350SioLayout.GPIO_OE_CLR_OFF;
        _bankResetMask = Rp2350Instances.IO_BANK0_RESET_MASK | Rp2350Instances.PADS_BANK0_RESET_MASK;
    }

    protected override int PinCount { get { return 32; } }

    protected override int ConvertPinNumberToLogicalNumberingScheme(int pinNumber) { return pinNumber; }

    protected override void OpenPin(int pinNumber) { }

    protected override void ClosePin(int pinNumber)
    {
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

    protected override void SetPinMode(int pinNumber, PinMode mode)
    {
        EnsureBankReady();
        Mmio.Write32(_ioCtrl0 + Rp2350IoBank0Layout.GPIO_CTRL_STRIDE * (uint)pinNumber,
            Rp2350IoBank0Layout.FUNCSEL_SIO);
        uint mask = 1u << pinNumber;
        uint pads = _pads0 + Rp2350PadsBank0Layout.GPIO_STRIDE * (uint)pinNumber;
        if (mode == PinMode.Output)
        {
            Mmio.Write32(pads, Rp2350PadsBank0Layout.GPIO0_IE);
            Mmio.Write32(_sioOeSet, mask);
        }
        else
        {
            uint pad = Rp2350PadsBank0Layout.GPIO0_IE;
            if (mode == PinMode.InputPullUp) pad |= Rp2350PadsBank0Layout.GPIO0_PUE;
            else if (mode == PinMode.InputPullDown) pad |= Rp2350PadsBank0Layout.GPIO0_PDE;
            Mmio.Write32(pads, pad);
            Mmio.Write32(_sioOeClr, mask);
        }
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
}
