// A Lamella.Hardware.AdcDriver for the SAMD21's analog-to-digital converter: twelve-bit counts,
// single-ended, one conversion at a time, each count a fraction of the board's analog supply.
//
// A channel number is the converter's own multiplexer code, so the numbers have gaps: AIN0 to AIN19
// are 0 to 19, and the internal inputs are the bandgap (25) and the core and I/O supplies, each
// scaled by a quarter (26 and 27). An AIN channel converts only on a pad the board's binding wires
// to the converter, because opening one hands its pad to the analog function and takes it from the
// PORT and from every other peripheral. The internal inputs are there on every board.
//
// Nothing is touched until a channel is first opened or read. Then the converter is brought up: its
// bus clock, its generic clock routed as DS40001882D 15.6.3.3 requires, a reset, the production
// calibration copied unchanged from NVM, half the analog supply as the reference at half gain so a
// full-scale count is the whole supply, and one conversion discarded, because the first after a
// reference change must not be used (33.6.2.1). A conversion that does not complete within its bound
// is reported as the seam's failure status, -3, never as a count.
//
// The converter halts while the core is halted by a debugger (33.5.7). This driver leaves DBGCTRL as
// it finds it.
using Lamella.Boards;
using Lamella.Generated;
using Lamella.Hardware;

public sealed class Samd21AdcDriver : AdcDriver
{
    // A bound on every hardware wait, so a converter that never answers is reported rather than
    // waited on forever.
    const int WaitBound = 100000;
    const int ConversionFailed = -3;

    readonly Samd21AdcBinding _binding;
    readonly uint _ctrla;
    readonly uint _refctrl;
    readonly uint _sampctrl;
    readonly uint _ctrlb;
    readonly uint _swtrig;
    readonly uint _inputctrl;
    readonly uint _intflag;
    readonly uint _status;
    readonly uint _result;
    readonly uint _calib;
    readonly uint _apbcmask;
    readonly uint _vref;
    readonly int _span;

    // Per wired pad: whether it is open, and its PMUX nibble and PINCFG byte as found, which closing
    // puts back.
    readonly bool[] _padOpen;
    readonly uint[] _padPmuxFound;
    readonly uint[] _padPincfgFound;
    bool _bandgapOpen;
    bool _bandgapRoutedBefore;
    bool _up;

    /// <summary>Binds the driver to the converter and the pads the board wired. No hardware is
    /// touched until a channel is first opened or read.</summary>
    public Samd21AdcDriver(Samd21AdcBinding binding)
    {
        _binding = binding;
        uint block = binding.AdcBase;
        _ctrla = block + Samd21AdcLayout.CTRLA_OFF;
        _refctrl = block + Samd21AdcLayout.REFCTRL_OFF;
        _sampctrl = block + Samd21AdcLayout.SAMPCTRL_OFF;
        _ctrlb = block + Samd21AdcLayout.CTRLB_OFF;
        _swtrig = block + Samd21AdcLayout.SWTRIG_OFF;
        _inputctrl = block + Samd21AdcLayout.INPUTCTRL_OFF;
        _intflag = block + Samd21AdcLayout.INTFLAG_OFF;
        _status = block + Samd21AdcLayout.STATUS_OFF;
        _result = block + Samd21AdcLayout.RESULT_OFF;
        _calib = block + Samd21AdcLayout.CALIB_OFF;
        _apbcmask = Samd21Instances.PM_BASE + Samd21PmLayout.APBCMASK_OFF;
        _vref = Samd21Instances.SYSCTRL_BASE + Samd21SysctrlLayout.VREF_OFF;
        // Every multiplexer code is below the MUXPOS field's span, whether or not it is a channel.
        _span = (int)(Samd21AdcLayout.INPUTCTRL_MUXPOS >> (int)Samd21AdcLayout.INPUTCTRL_MUXPOS_LSB) + 1;
        int pads = binding.PadCount;
        _padOpen = new bool[pads];
        _padPmuxFound = new uint[pads];
        _padPincfgFound = new uint[pads];
    }

    /// <summary>The span of the channel numbers, 32: every multiplexer code is below it. Which of
    /// them convert on this board, <see cref="IsChannelSupported"/> says.</summary>
    public override int ChannelCount { get { return _span; } }

    /// <summary>Whether <paramref name="channel"/> converts on this board: an internal input, or an
    /// AIN channel whose pad the board wired to the converter.</summary>
    public override bool IsChannelSupported(int channel)
    {
        if (!Samd21AdcLayout.IsChannel(channel))
        {
            return false;
        }
        // The AIN channels are the multiplexer's lowest codes, and the internal inputs sit above
        // them (33.8.8).
        return channel > Samd21AdcLayout.Channel_AIN19 || PadSlot(channel) >= 0;
    }

    /// <summary>Twelve bits, the resolution this driver selects.</summary>
    public override int ResolutionInBits { get { return (int)Samd21AdcLayout.ResolutionBits; } }

    public override int MinValue { get { return (int)Samd21AdcLayout.MinValue; } }

    public override int MaxValue { get { return (int)Samd21AdcLayout.MaxValue; } }

    /// <summary>What a full-scale count means, in microvolts: the board's analog supply.</summary>
    public int ReferenceMicrovolts { get { return (int)_binding.ReferenceMicrovolts; } }

    /// <summary>Single-ended only. A differential conversion needs a second pad wired to the
    /// negative input, and the binding describes each pad as an input of its own.</summary>
    public override bool IsChannelModeSupported(AdcChannelMode mode)
    {
        return mode == AdcChannelMode.SingleEnded;
    }

    public override void SetChannelMode(AdcChannelMode mode)
    {
        if (mode != AdcChannelMode.SingleEnded)
        {
            throw new System.NotSupportedException("this converter driver is single-ended only");
        }
    }

    /// <summary>Claims a channel: an AIN channel's pad is handed to the analog function, and the
    /// bandgap is routed to the converter. Opening an open channel does nothing.</summary>
    /// <exception cref="System.ArgumentOutOfRangeException">The channel does not convert on this
    /// board.</exception>
    public override void OpenChannel(int channel)
    {
        if (!IsChannelSupported(channel))
        {
            throw new System.ArgumentOutOfRangeException("channel");
        }
        EnsureUp();
        int slot = PadSlot(channel);
        if (slot >= 0)
        {
            if (_padOpen[slot])
            {
                return;
            }
            Samd21AdcPad pad = _binding.Pad(slot);
            uint mask = Samd21PortLayout.PMUX0_PMUXE << (int)pad.PmuxShift;
            uint pmux = Mmio.Read8(pad.PmuxReg);
            _padPmuxFound[slot] = pmux & mask;
            _padPincfgFound[slot] = Mmio.Read8(pad.PincfgReg);
            // The function first and PMUXEN second, so the pad never runs another peripheral's
            // function between the two writes. PMUXEN alone: an input buffer or a pull left on
            // would load the signal being measured.
            Mmio.Write8(pad.PmuxReg, (byte)((pmux & ~mask) | (_binding.PmuxFunc << (int)pad.PmuxShift)));
            Mmio.Write8(pad.PincfgReg, (byte)Samd21PortLayout.PINCFG0_PMUXEN);
            _padOpen[slot] = true;
        }
        else if (channel == Samd21AdcLayout.Channel_Bandgap && !_bandgapOpen)
        {
            // The bandgap reaches the converter only with VREF.BGOUTEN set (17.8.16). The rest of
            // VREF holds the factory's bandgap calibration, so the bit is set by read-modify-write.
            uint vref = Mmio.Read32(_vref);
            _bandgapRoutedBefore = (vref & Samd21SysctrlLayout.VREF_BGOUTEN) != 0u;
            Mmio.Write32(_vref, vref | Samd21SysctrlLayout.VREF_BGOUTEN);
            _bandgapOpen = true;
        }
    }

    /// <summary>Releases a channel: an AIN pad gets back the multiplexer setting and pin
    /// configuration it had when opened, and the bandgap is unrouted unless it was routed before.
    /// Releasing a channel that is not open does nothing.</summary>
    public override void CloseChannel(int channel)
    {
        int slot = PadSlot(channel);
        if (slot >= 0)
        {
            if (!_padOpen[slot])
            {
                return;
            }
            Samd21AdcPad pad = _binding.Pad(slot);
            uint mask = Samd21PortLayout.PMUX0_PMUXE << (int)pad.PmuxShift;
            // PINCFG first: a pad the PORT owned before the open goes straight back to the PORT,
            // and the nibble written after it is not seen.
            Mmio.Write8(pad.PincfgReg, (byte)_padPincfgFound[slot]);
            uint pmux = Mmio.Read8(pad.PmuxReg);
            Mmio.Write8(pad.PmuxReg, (byte)((pmux & ~mask) | _padPmuxFound[slot]));
            _padOpen[slot] = false;
        }
        else if (channel == Samd21AdcLayout.Channel_Bandgap && _bandgapOpen)
        {
            if (!_bandgapRoutedBefore)
            {
                Mmio.Write32(_vref, Mmio.Read32(_vref) & ~Samd21SysctrlLayout.VREF_BGOUTEN);
            }
            _bandgapOpen = false;
        }
    }

    /// <summary>Converts one channel and returns its count, from 0 to 4095 of the analog supply, or
    /// -3 when the converter could not produce one.</summary>
    /// <exception cref="System.ArgumentOutOfRangeException">The channel does not convert on this
    /// board.</exception>
    /// <exception cref="System.InvalidOperationException">The channel is a pad or the bandgap and
    /// is not open, so its input is not routed to the converter.</exception>
    public override int ReadValue(int channel)
    {
        if (!IsChannelSupported(channel))
        {
            throw new System.ArgumentOutOfRangeException("channel");
        }
        int slot = PadSlot(channel);
        if ((slot >= 0 && !_padOpen[slot]) || (channel == Samd21AdcLayout.Channel_Bandgap && !_bandgapOpen))
        {
            throw new System.InvalidOperationException("the channel is not open, so its input is not routed to the converter");
        }
        if (!EnsureUp())
        {
            return ConversionFailed;
        }
        return Convert(channel);
    }

    // Brings the converter up once. False when a step did not complete within its bound; the next
    // call then starts again from the beginning.
    bool EnsureUp()
    {
        if (_up)
        {
            return true;
        }
        // The bus clock, then the converter's own clock. Without the first, every register below
        // reads as zero and ignores writes (14.4).
        Mmio.Write32(_apbcmask, Mmio.Read32(_apbcmask) | _binding.ApbcMask);
        if (!Samd21GenericClock.Route(_binding.GclkClkctrlValue))
        {
            return false;
        }

        // A reset, so nothing is inherited, with the converter disabled first as the reset requires
        // (33.6.2.2). The reset spares DBGCTRL.
        uint ctrla = Mmio.Read8(_ctrla);
        if ((ctrla & Samd21AdcLayout.CTRLA_ENABLE) != 0u)
        {
            Mmio.Write8(_ctrla, (byte)(ctrla & ~Samd21AdcLayout.CTRLA_ENABLE));
            if (!WaitSync())
            {
                return false;
            }
        }
        Mmio.Write8(_ctrla, (byte)Samd21AdcLayout.CTRLA_SWRST);
        if (!WaitReset())
        {
            return false;
        }

        // The production calibration, copied unchanged (33.8.19).
        uint linearity = CalibrationBits(Samd21AdcLayout.NVM_LINEARITY_LSB, Samd21AdcLayout.NVM_LINEARITY_WIDTH);
        uint bias = CalibrationBits(Samd21AdcLayout.NVM_BIASCAL_LSB, Samd21AdcLayout.NVM_BIASCAL_WIDTH);
        Mmio.Write16(_calib, (ushort)((linearity << (int)Samd21AdcLayout.CALIB_LINEARITY_CAL_LSB)
            | (bias << (int)Samd21AdcLayout.CALIB_BIAS_CAL_LSB)));

        // Half the analog supply as the reference, which Table 33-5 allows above 2.0 V of VDDANA.
        // Each conversion runs at half gain, so a full-scale count is the whole supply.
        Mmio.Write8(_refctrl, (byte)(Samd21AdcLayout.REFSEL_INTVCC1 << (int)Samd21AdcLayout.REFCTRL_REFSEL_LSB));
        // The longest sampling time the field holds, 64 half cycles of the converter clock, so a
        // source behind a high resistance still charges the sampling capacitor. The bandgap also
        // requires this field to be written (33.8.8).
        Mmio.Write8(_sampctrl, (byte)Samd21AdcLayout.SAMPCTRL_SAMPLEN);
        // Twelve bits, right-adjusted, one conversion per start, at the binding's prescaler.
        Mmio.Write16(_ctrlb, (ushort)((_binding.Prescaler << (int)Samd21AdcLayout.CTRLB_PRESCALER_LSB)
            | (Samd21AdcLayout.RESSEL_12BIT << (int)Samd21AdcLayout.CTRLB_RESSEL_LSB)));
        if (!WaitSync())
        {
            return false;
        }
        Mmio.Write8(_ctrla, (byte)Samd21AdcLayout.CTRLA_ENABLE);
        if (!WaitSync())
        {
            return false;
        }
        // The first conversion after the reference changes must not be used (33.6.2.1).
        if (Convert(Samd21AdcLayout.Channel_ScaledIoSupply) < 0)
        {
            return false;
        }
        _up = true;
        return true;
    }

    // One single-ended conversion at half gain against the internal ground: select the input,
    // start, and wait for the result.
    int Convert(int channel)
    {
        uint select = ((uint)channel << (int)Samd21AdcLayout.INPUTCTRL_MUXPOS_LSB)
            | (Samd21AdcLayout.MUXNEG_GND << (int)Samd21AdcLayout.INPUTCTRL_MUXNEG_LSB)
            | (Samd21AdcLayout.GAIN_DIV2 << (int)Samd21AdcLayout.INPUTCTRL_GAIN_LSB);
        Mmio.Write32(_inputctrl, select);
        if (!WaitSync())
        {
            return ConversionFailed;
        }
        // A flag left by a conversion nobody read would end the wait below at once.
        Mmio.Write8(_intflag, (byte)(Samd21AdcLayout.INTFLAG_RESRDY | Samd21AdcLayout.INTFLAG_OVERRUN));
        Mmio.Write8(_swtrig, (byte)Samd21AdcLayout.SWTRIG_START);
        if (!WaitSync())
        {
            return ConversionFailed;
        }
        for (int spin = 0; spin < WaitBound; spin++)
        {
            if ((Mmio.Read8(_intflag) & Samd21AdcLayout.INTFLAG_RESRDY) != 0u)
            {
                // Reading RESULT is what clears RESRDY.
                return (int)(Mmio.Read16(_result) & Samd21AdcLayout.RESULT_RESULT);
            }
        }
        return ConversionFailed;
    }

    // The registers written with synchronization (33.6.15) finish when STATUS.SYNCBUSY clears.
    bool WaitSync()
    {
        for (int spin = 0; spin < WaitBound; spin++)
        {
            if ((Mmio.Read8(_status) & Samd21AdcLayout.STATUS_SYNCBUSY) == 0u)
            {
                return true;
            }
        }
        return false;
    }

    // A software reset is complete when CTRLA.SWRST and STATUS.SYNCBUSY have both cleared (33.8.1).
    bool WaitReset()
    {
        for (int spin = 0; spin < WaitBound; spin++)
        {
            if ((Mmio.Read8(_ctrla) & Samd21AdcLayout.CTRLA_SWRST) == 0u
                && (Mmio.Read8(_status) & Samd21AdcLayout.STATUS_SYNCBUSY) == 0u)
            {
                return true;
            }
        }
        return false;
    }

    // The index of the wired pad that is this channel, or -1 when no wired pad is.
    int PadSlot(int channel)
    {
        for (int i = 0; i < _binding.PadCount; i++)
        {
            if (_binding.Pad(i).Channel == channel)
            {
                return i;
            }
        }
        return -1;
    }

    // Bits lsb to lsb + width - 1 of the NVM software calibration area, numbered from bit 0 of its
    // first word as Table 10-5 numbers them. A field may continue into the next word.
    static uint CalibrationBits(uint lsb, uint width)
    {
        uint word = Samd21AdcLayout.NVM_CALIBRATION_AREA + (lsb / 32u) * 4u;
        int shift = (int)(lsb % 32u);
        uint bits = Mmio.Read32(word) >> shift;
        if (shift + (int)width > 32)
        {
            bits = bits | (Mmio.Read32(word + 4u) << (32 - shift));
        }
        return bits & ((1u << (int)width) - 1u);
    }
}
