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
    private readonly uint _intr0;
    private readonly uint _inte0;

    private readonly uint _reserved;

    private readonly int[] _pairedPins;
    private readonly int[] _partnerPads;

    /// <summary>Reserves nothing -- every pin is the app's.</summary>
    public Rp2350GpioDriver() : this(0u) { }

    /// <summary>Reserves the pins whose bits are set, so <c>SetPinMode</c> refuses them. A board
    /// passes the lines it must keep -- its debug transport, a fixed peripheral's control pin --
    /// because only the board knows which those are.</summary>
    /// <remarks>THE PICO 2 W IS WHY THIS EXISTS ON THIS FAMILY. Its wireless part sits on GP23,
    /// GP24, GP25 and GP29, and GP25 is the pin that drives the user LED on a Pico 2. So the line
    /// that blinks a Pico 2 asserts the radio's chip select on a Pico 2 W, from identical source and
    /// with nothing to see.</remarks>
    public Rp2350GpioDriver(uint reserved) : this(reserved, new int[0], new int[0]) { }

    /// <summary>Reserves the pins whose bits are set, as <see cref="Rp2350GpioDriver(uint)"/> does,
    /// and pairs each of <paramref name="pairedPins"/> with the pad at the same index of
    /// <paramref name="partnerPads"/>, a second pad the board wires to the same net. Setting a mode
    /// on a paired pin first leaves its partner high-impedance with no pull, so the partner neither
    /// drives the line nor loads it.</summary>
    /// <remarks>THE PIMORONI PICO PLUS 2 BOARDS ARE WHY. Their header positions 26 to 28 reach
    /// GPIO26 to GPIO28 and, through 1k, the converter's pads GPIO40 to GPIO42. A pad keeps its
    /// reset pull-down until something configures it, so without this a pulled-up input on one of
    /// those positions sits near half the rail.</remarks>
    /// <exception cref="System.ArgumentException">The two arrays differ in length.</exception>
    public Rp2350GpioDriver(uint reserved, int[] pairedPins, int[] partnerPads)
    {
        if (pairedPins.Length != partnerPads.Length)
        {
#if LAMELLA_CORLIB_LINKED
            throw new System.ArgumentException("each paired pin needs exactly one partner pad");
#else
            throw new System.Exception("each paired pin needs exactly one partner pad");
#endif
        }
        _reserved = reserved;
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

    protected override int PinCount { get { return 32; } }

    protected override int ConvertPinNumberToLogicalNumberingScheme(int pinNumber) { return pinNumber; }

    protected override void OpenPin(int pinNumber) { }

    protected override void ClosePin(int pinNumber)
    {
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
        ParkPartner(pinNumber);
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

    void ParkPartner(int pinNumber)
    {
        for (int i = 0; i < _pairedPins.Length; i++)
        {
            if (_pairedPins[i] != pinNumber) continue;
            uint partner = (uint)_partnerPads[i];
            Mmio.Write32(_ioCtrl0 + Rp2350IoBank0Layout.GPIO_CTRL_STRIDE * partner, Rp2350IoBank0Layout.FUNCSEL_NULL);
            Mmio.Write32(_pads0 + Rp2350PadsBank0Layout.GPIO_STRIDE * partner, Rp2350PadsBank0Layout.GPIO0_OD);
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

    /// <summary>Registers <paramref name="callback"/> for the edges in <paramref name="eventTypes"/> on
    /// the pin, and enables the pin's edge interrupt. The pad's input buffer is enabled if it was not,
    /// so the level reported with each event is the pad's.</summary>
    /// <exception cref="System.ArgumentException"><paramref name="pinNumber"/> is not a pin of this
    /// driver or is reserved by the board, or <paramref name="eventTypes"/> names neither a rising nor
    /// a falling edge.</exception>
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
    /// <exception cref="System.ArgumentException"><paramref name="pinNumber"/> is not a pin of this
    /// driver.</exception>
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
            ParkPartner(pinNumber);
            uint pads = _pads0 + Rp2350PadsBank0Layout.GPIO_STRIDE * (uint)pinNumber;
            Mmio.Write32(pads, (Mmio.Read32(pads) | Rp2350PadsBank0Layout.GPIO0_IE) & ~Rp2350PadsBank0Layout.GPIO0_ISO);
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

    static void CheckPin(int pinNumber)
    {
        if (pinNumber < 0 || pinNumber >= 32)
        {
            BadArgument("pinNumber is not a pin of this driver: GP0..GP31 are 0..31");
        }
    }

    static void BadArgument(string why)
    {
#if LAMELLA_CORLIB_LINKED
        throw new System.ArgumentException(why);
#else
        throw new System.Exception(why);
#endif
    }
}
