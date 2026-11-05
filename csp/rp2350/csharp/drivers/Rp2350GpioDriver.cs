// A System.Device.Gpio driver for the RP2350's GPIOs, GP0 to GP29 on the QFN-60 package and GP0 to GP47 on the QFN-80, over
using System.Device.Gpio;
using Lamella.Generated;
using Lamella.Hardware;

internal sealed class Rp2350GpioDriver : GpioDriver
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
    private readonly uint _intr0;
    private readonly uint _inte0;

    const uint Bank1Stride = Rp2350SioLayout.GPIO_HI_OUT_SET_OFF - Rp2350SioLayout.GPIO_OUT_SET_OFF;

    private readonly uint _reserved;
    private readonly uint _reservedBank1;

    private int _pinCount;

    private readonly int[] _pairedPins;
    private readonly int[] _partnerPads;

    /// <summary>Reserves nothing -- every pin is the app's.</summary>
    public Rp2350GpioDriver() : this(0u) { }

    /// <summary>Reserves the bank-0 pins whose bits are set, so <c>SetPinMode</c> refuses them. A board
    /// passes the lines it must keep -- its debug transport, a fixed peripheral's control pin --
    /// because only the board knows which those are.</summary>
    /// <remarks>The Pico 2 W is why this exists on this family. Its wireless part sits on GP23,
    /// GP24, GP25 and GP29, and GP25 is the pin that drives the user LED on a Pico 2. So the line
    /// that blinks a Pico 2 asserts the radio's chip select on a Pico 2 W, from identical source and
    /// with nothing to see.</remarks>
    public Rp2350GpioDriver(uint reserved) : this(reserved, new int[0], new int[0]) { }

    /// <summary>Reserves the pins whose bits are set, as <see cref="Rp2350GpioDriver(uint)"/> does,
    /// and pairs each of <paramref name="pairedPins"/> with the pad at the same index of
    /// <paramref name="partnerPads"/>, a second pad the board wires to the same net. Setting a mode
    /// on either pad first parks the other if nothing holds it, high-impedance with no pull, so it
    /// neither drives the line nor loads it. Where the other holds the net, it is left as it is, so
    /// one pad can read what the other drives; a second driver on the net, or a pull against the other's
    /// pull, is refused.</summary>
    /// <remarks>The Pimoroni Pico Plus 2 boards are why. Their header positions 26 to 28 reach
    /// GPIO26 to GPIO28 and, through 1k, the converter's pads GPIO40 to GPIO42. A pad keeps its
    /// reset pull-down until something configures it, so without this a pulled-up input on one of
    /// those positions sits well below the rail.</remarks>
    /// <exception cref="System.ArgumentException">The two arrays differ in length.</exception>
    public Rp2350GpioDriver(uint reserved, int[] pairedPins, int[] partnerPads)
        : this(reserved, 0u, pairedPins, partnerPads) { }

    /// <summary>Reserves pins of both banks and pairs pads, as
    /// <see cref="Rp2350GpioDriver(uint, int[], int[])"/> does: <paramref name="reservedBank1"/> holds GP32
    /// in bit 0 and GP47 in bit 15.</summary>
    /// <remarks>The Pimoroni boards wire their PSRAM's chip select to GP47, in bank 1, and a program that
    /// took that line would stop the memory the PSRAM serves.</remarks>
    /// <exception cref="System.ArgumentException">The two arrays differ in length.</exception>
    public Rp2350GpioDriver(uint reserved, uint reservedBank1, int[] pairedPins, int[] partnerPads)
    {
        if (pairedPins.Length != partnerPads.Length)
        {
            throw new System.ArgumentException("each paired pin needs exactly one partner pad");
        }
        _reserved = reserved;
        _reservedBank1 = reservedBank1;
        _pairedPins = pairedPins;
        _partnerPads = partnerPads;
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
        _intr0 = Rp2350Instances.IO_BANK0_BASE + Rp2350IoBank0Layout.INTR0_OFF;
        _inte0 = Rp2350Instances.IO_BANK0_BASE + Rp2350IoBank0Layout.PROC0_INTE0_OFF;
    }

    protected override int PinCount
    {
        get
        {
            if (_pinCount == 0)
            {
                uint package = Mmio.Read32(Rp2350Instances.SYSINFO_BASE + Rp2350SysinfoLayout.PACKAGE_SEL_OFF);
                _pinCount = (package & Rp2350SysinfoLayout.PACKAGE_SEL_QFN60) != 0u ? 30 : 48;
            }
            return _pinCount;
        }
    }

    /// <exception cref="System.ArgumentOutOfRangeException"><paramref name="pinNumber"/> is not a GPIO of
    /// this chip's package.</exception>
    protected override void OpenPin(int pinNumber)
    {
        CheckPin(pinNumber);
    }

    protected override void ClosePin(int pinNumber)
    {
        if (pinNumber < 0 || pinNumber >= PinCount) return;
        if (IsReserved(pinNumber)) return;

        if (IsArmed(pinNumber))
        {
            for (int edges = 1; edges <= EdgeSets; edges = edges + 1)
            {
                PinChangeEventHandler row = _rows[RowOf(pinNumber, edges)];
                if ((object)row != null)
                {
                    PinEvents.Unregister(pinNumber, row);
                    RemoveFromRows(pinNumber, row);
                }
            }
            Sense(pinNumber, 0);
        }
        Mmio.Write32(BankOf(_sioOeClr, pinNumber), MaskOf(pinNumber));
        if (TwinOf(pinNumber) >= 0)
        {
            EnsureBankReady();
            Rp2350TwinPads.ParkUnlessPwm(pinNumber);
        }
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
        return ((pinNumber < 32 ? _reserved : _reservedBank1) & MaskOf(pinNumber)) != 0u;
    }

    private static uint BankOf(uint register, int pinNumber)
    {
        return pinNumber < 32 ? register : register + Bank1Stride;
    }

    private static uint MaskOf(int pinNumber)
    {
        return 1u << (pinNumber & 31);
    }

    /// <exception cref="System.ArgumentOutOfRangeException"><paramref name="pinNumber"/> is not a GPIO of
    /// this chip's package.</exception>
    /// <exception cref="System.ArgumentException">The board reserves the pin.</exception>
    /// <exception cref="System.InvalidOperationException">The board wires the pin and a second pad to one net,
    /// and the mode would drive that net while the second pad drives it, or would pull it against the second
    /// pad's pull. The message names both pads, and nothing is written.</exception>
    protected override void SetPinMode(int pinNumber, PinMode mode)
    {
        CheckPin(pinNumber);
        if (IsReserved(pinNumber))
        {
            throw new System.ArgumentException("pin is reserved by the board");
        }
        EnsureBankReady();
        uint pull = 0u;
        if (mode == PinMode.InputPullUp) pull = Rp2350PadsBank0Layout.GPIO0_PUE;
        else if (mode == PinMode.InputPullDown) pull = Rp2350PadsBank0Layout.GPIO0_PDE;
        int twin = TwinOf(pinNumber);
        if (twin >= 0)
        {
            Rp2350TwinPads.Claim(pinNumber, twin, mode == PinMode.Output, pull);
        }
        Mmio.Write32(_ioCtrl0 + Rp2350IoBank0Layout.GPIO_CTRL_STRIDE * (uint)pinNumber,
            Rp2350IoBank0Layout.FUNCSEL_SIO);
        Mmio.Write32(_pads0 + Rp2350PadsBank0Layout.GPIO_STRIDE * (uint)pinNumber, Rp2350PadsBank0Layout.GPIO0_IE | pull);
        Mmio.Write32(BankOf(mode == PinMode.Output ? _sioOeSet : _sioOeClr, pinNumber), MaskOf(pinNumber));
    }

    int TwinOf(int pinNumber)
    {
        for (int i = 0; i < _pairedPins.Length; i++)
        {
            if (_pairedPins[i] == pinNumber) return _partnerPads[i];
            if (_partnerPads[i] == pinNumber) return _pairedPins[i];
        }
        return -1;
    }

    protected override PinMode GetPinMode(int pinNumber)
    {
        CheckBank1(pinNumber);
        return (Mmio.Read32(BankOf(_sioOe, pinNumber)) & MaskOf(pinNumber)) != 0u ? PinMode.Output : PinMode.Input;
    }

    protected override bool IsPinModeSupported(int pinNumber, PinMode mode) { return true; }

    protected override PinValue Read(int pinNumber)
    {
        CheckBank1(pinNumber);
        return (int)((Mmio.Read32(BankOf(_sioIn, pinNumber)) >> (pinNumber & 31)) & 1u);
    }

    protected override void Write(int pinNumber, PinValue value)
    {
        CheckBank1(pinNumber);
        Mmio.Write32(BankOf((bool)value ? _sioOutSet : _sioOutClr, pinNumber), MaskOf(pinNumber));
    }

    protected override void Toggle(int pinNumber)
    {
        CheckBank1(pinNumber);
        Mmio.Write32(BankOf(_sioOutXor, pinNumber), MaskOf(pinNumber));
    }

    void CheckBank1(int pinNumber)
    {
        if ((uint)pinNumber >= 32u) CheckPin(pinNumber);
    }

    /// <summary>Registers <paramref name="callback"/> for the edges in <paramref name="eventTypes"/> on
    /// the pin, and enables the pin's edge interrupt. The pad's input buffer is enabled if it was not,
    /// so the level reported with each event is the pad's.</summary>
    /// <exception cref="System.ArgumentOutOfRangeException"><paramref name="pinNumber"/> is not a GPIO of
    /// this chip's package.</exception>
    /// <exception cref="System.ArgumentException"><paramref name="pinNumber"/> is reserved by the board, or
    /// <paramref name="eventTypes"/> names neither a rising nor a falling edge.</exception>
    protected override void AddCallbackForPinValueChangedEvent(
        int pinNumber, PinEventTypes eventTypes, PinChangeEventHandler callback)
    {
        CheckPin(pinNumber);
        if (IsReserved(pinNumber))
        {
            BadArgument("pin is reserved by the board");
        }
        int edges = (int)eventTypes & ((int)PinEventTypes.Rising | (int)PinEventTypes.Falling);
        if (edges == 0)
        {
            BadArgument("eventTypes names neither a rising nor a falling edge");
        }
        PinEvents.Register(this, pinNumber, pinNumber, (PinEventTypes)edges, callback);
        if (_rows == null)
        {
            _rows = new PinChangeEventHandler[PinCount * EdgeSets];
            _armed = new int[PinCount];
        }
        int row = RowOf(pinNumber, edges);
        _rows[row] = (PinChangeEventHandler)System.Delegate.Combine(_rows[row], callback);
        Sense(pinNumber, _armed[pinNumber] | edges);
    }

    /// <summary>Removes <paramref name="callback"/> from the pin, for every edge it was registered for,
    /// and disables the pin's edge interrupt once no callback remains on it.</summary>
    /// <exception cref="System.ArgumentOutOfRangeException"><paramref name="pinNumber"/> is not a GPIO of
    /// this chip's package.</exception>
    protected override void RemoveCallbackForPinValueChangedEvent(int pinNumber, PinChangeEventHandler callback)
    {
        CheckPin(pinNumber);
        PinEvents.Unregister(pinNumber, callback);
        if (!IsArmed(pinNumber))
        {
            return;
        }
        RemoveFromRows(pinNumber, callback);
        Sense(pinNumber, EdgesInRows(pinNumber));
    }

    private PinChangeEventHandler[] _rows;
    private int[] _armed;
    const int EdgeSets = 3;

    static int RowOf(int pinNumber, int edges)
    {
        return pinNumber * EdgeSets + edges - 1;
    }

    bool IsArmed(int pinNumber)
    {
        return _armed != null && pinNumber >= 0 && pinNumber < _armed.Length && _armed[pinNumber] != 0;
    }

    void RemoveFromRows(int pinNumber, PinChangeEventHandler callback)
    {
        for (int edges = 1; edges <= EdgeSets; edges = edges + 1)
        {
            int row = RowOf(pinNumber, edges);
            _rows[row] = (PinChangeEventHandler)System.Delegate.Remove(_rows[row], callback);
        }
    }

    int EdgesInRows(int pinNumber)
    {
        int edges = 0;
        for (int set = 1; set <= EdgeSets; set = set + 1)
        {
            if ((object)_rows[RowOf(pinNumber, set)] != null)
            {
                edges = edges | set;
            }
        }
        return edges;
    }

    void Sense(int pinNumber, int edges)
    {
        int was = _armed[pinNumber];
        if (edges == was)
        {
            return;
        }
        uint register = (uint)(pinNumber / (int)Rp2350IoBank0Layout.GPIO_INT_PINS) * Rp2350IoBank0Layout.GPIO_INT_STRIDE;
        int shift = (pinNumber % (int)Rp2350IoBank0Layout.GPIO_INT_PINS) * (int)Rp2350IoBank0Layout.GPIO_INT_BITS;
        if (was == 0)
        {
            EnsureBankReady();
            uint pads = _pads0 + Rp2350PadsBank0Layout.GPIO_STRIDE * (uint)pinNumber;
            uint pad = Mmio.Read32(pads);
            int twin = TwinOf(pinNumber);
            if (twin >= 0)
            {
                Rp2350TwinPads.ParkIfIdle(twin);
                if ((pad & Rp2350PadsBank0Layout.GPIO0_ISO) != 0u)
                {
                    pad = pad & ~(Rp2350PadsBank0Layout.GPIO0_PUE | Rp2350PadsBank0Layout.GPIO0_PDE);
                }
            }
            Mmio.Write32(pads, (pad | Rp2350PadsBank0Layout.GPIO0_IE) & ~Rp2350PadsBank0Layout.GPIO0_ISO);
        }
        Mmio.Write32(_intr0 + register, EdgeBits(edges & ~was) << shift);
        uint enables = Mmio.Read32(_inte0 + register) & ~(EdgeBits(Both) << shift);
        Mmio.Write32(_inte0 + register, enables | (EdgeBits(edges) << shift));
        _armed[pinNumber] = edges;
    }

    const int Both = (int)PinEventTypes.Rising | (int)PinEventTypes.Falling;

    static uint EdgeBits(int edges)
    {
        uint bits = 0u;
        if ((edges & (int)PinEventTypes.Rising) != 0) bits = bits | Rp2350IoBank0Layout.GPIO_INT_EDGE_HIGH;
        if ((edges & (int)PinEventTypes.Falling) != 0) bits = bits | Rp2350IoBank0Layout.GPIO_INT_EDGE_LOW;
        return bits;
    }

    void CheckPin(int pinNumber)
    {
        if (pinNumber < 0 || pinNumber >= PinCount)
        {
            throw new System.ArgumentOutOfRangeException("pinNumber",
                "This chip's package has GP0 to GP" + (PinCount - 1).ToString() + ".");
        }
    }

    static void BadArgument(string why)
    {
        throw new System.ArgumentException(why);
    }
}
