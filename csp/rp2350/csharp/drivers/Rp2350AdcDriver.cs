// A Lamella.Hardware.AdcDriver for the RP2350 on-chip SAR ADC, over Lamella.Hardware.Mmio --
using System;
using Lamella.Boards;
using Lamella.Generated;
using Lamella.Hardware;

public sealed class Rp2350AdcDriver : AdcDriver
{
    private readonly uint _cs;
    private readonly uint _result;
    private readonly uint _clkAdcCtrl;
    private readonly uint _resetsClr;
    private readonly uint _resetsDone;
    private readonly uint _ioCtrl0;
    private readonly uint _pads0;
    private readonly int _channelCount;
    private readonly int _resolutionBits;
    private readonly int _minValue;
    private readonly int _maxValue;
    private readonly Rp2350AdcBinding _binding;
    private bool _up;

    /// <summary>Binds the driver to the converter. No hardware is touched until a channel is first
    /// opened or read, which is when the converter is brought up.</summary>
    /// <exception cref="ArgumentException">The binding's channel map is inconsistent: every pin channel
    /// needs one entry in each of its tables, and the temperature sensor is the channel after
    /// them.</exception>
    public Rp2350AdcDriver(Rp2350AdcBinding binding)
    {
        int pins = binding.ChannelPins.Length;
        if (binding.TemperatureChannel != pins || binding.ChannelCount != pins + 1 || binding.ReservedBy.Length != pins
            || binding.TwinIoCtrl.Length != pins || binding.TwinPads.Length != pins)
        {
            throw new ArgumentException("the ADC binding's channel map is inconsistent: every pin channel needs one entry "
                + "in each of its tables, and the temperature sensor is the channel after them");
        }
        _binding = binding;
        _cs = binding.AdcBase + Rp2350AdcLayout.CS_OFF;
        _result = binding.AdcBase + Rp2350AdcLayout.RESULT_OFF;
        _clkAdcCtrl = Rp2350Instances.CLOCKS_BASE + Rp2350ClocksLayout.CLK_ADC_CTRL_OFF;
        _resetsClr = Rp2350Instances.RESETS_CLR_BASE + Rp2350ResetsLayout.RESET_OFF;
        _resetsDone = Rp2350Instances.RESETS_BASE + Rp2350ResetsLayout.RESET_DONE_OFF;
        _ioCtrl0 = Rp2350Instances.IO_BANK0_BASE + Rp2350IoBank0Layout.GPIO0_CTRL_OFF;
        _pads0 = Rp2350Instances.PADS_BANK0_BASE + Rp2350PadsBank0Layout.GPIO0_OFF;
        _channelCount = binding.ChannelCount;
        _resolutionBits = (int)Rp2350AdcLayout.ResolutionBits;
        _minValue = (int)Rp2350AdcLayout.MinValue;
        _maxValue = (int)Rp2350AdcLayout.MaxValue;
    }

    bool EnsureUp()
    {
        if (_up) return true;

        uint auxPllUsb = Rp2350ClocksLayout.CLK_ADC_AUXSRC_PLL_USB << (int)Rp2350ClocksLayout.CLK_ADC_CTRL_AUXSRC_LSB;
        uint ctrl = Mmio.Read32(_clkAdcCtrl);
        bool running = (ctrl & Rp2350ClocksLayout.CLK_ADC_CTRL_ENABLED) != 0u;
        bool onPllUsb = (ctrl & Rp2350ClocksLayout.CLK_ADC_CTRL_AUXSRC) == auxPllUsb;
        if (!running || !onPllUsb)
        {
            Mmio.Write32(_clkAdcCtrl, ctrl & ~Rp2350ClocksLayout.CLK_ADC_CTRL_ENABLE);
            if (!WaitClock(false)) return false;
            Mmio.Write32(_clkAdcCtrl, auxPllUsb);
            Mmio.Write32(_clkAdcCtrl, auxPllUsb | Rp2350ClocksLayout.CLK_ADC_CTRL_ENABLE);
            if (!WaitClock(true)) return false;
        }

        Mmio.Write32(_resetsClr, _binding.ResetMask);
        bool released = false;
        for (int spin = 0; spin < 100000; spin++)
        {
            if ((Mmio.Read32(_resetsDone) & _binding.ResetMask) == _binding.ResetMask)
            {
                released = true;
                break;
            }
        }
        if (!released) return false;

        Mmio.Write32(_cs, Rp2350AdcLayout.CS_EN | Rp2350AdcLayout.CS_TS_EN);
        if (!WaitReady()) return false;
        _up = true;
        return true;
    }

    bool WaitClock(bool enabled)
    {
        for (int spin = 0; spin < 100000; spin++)
        {
            if (((Mmio.Read32(_clkAdcCtrl) & Rp2350ClocksLayout.CLK_ADC_CTRL_ENABLED) != 0u) == enabled) return true;
        }
        return false;
    }

    /// <summary>The board's reference rail in microvolts (the binding's board truth) --
    /// what a raw count converts against.</summary>
    public int ReferenceMicrovolts { get { return (int)_binding.ReferenceMicrovolts; } }

    /// <summary>The channels of the board's package: its pin channels, then the temperature sensor
    /// (five on the QFN-60, nine on the QFN-80).</summary>
    public override int ChannelCount { get { return _channelCount; } }

    /// <summary>12-bit SAR (ENOB min 9 / typ 9.5 per the datasheet's electrical table).</summary>
    public override int ResolutionInBits { get { return _resolutionBits; } }

    const int ConversionFailed = -3;

    public override int MinValue { get { return _minValue; } }

    public override int MaxValue { get { return _maxValue; } }

    /// <summary>The RP2350 converter is single-ended only.</summary>
    public override bool IsChannelModeSupported(AdcChannelMode mode)
    {
        return mode == AdcChannelMode.SingleEnded;
    }

    public override void SetChannelMode(AdcChannelMode mode)
    {
        if (mode != AdcChannelMode.SingleEnded)
        {
            throw new ArgumentException("rp2350 adc is single-ended only");
        }
    }

    /// <summary>Pin channels get the pad_analog prep (digital receiver off, output disabled,
    /// pulls off, no digital function), and so does a second GPIO the board wires to the same
    /// header pin, so it cannot load or drive what the channel reads. The temperature channel
    /// needs none -- its bias rides CS.TS_EN from init.</summary>
    /// <exception cref="InvalidOperationException">The channel's pin belongs to another line on
    /// this board; nothing is written.</exception>
    public override void OpenChannel(int channel)
    {
        int pin = ChannelPin(channel);
        if (pin >= 0 && (_binding.ReservedChannels & (1u << channel)) != 0u)
        {
            throw new InvalidOperationException("ADC channel " + channel + " reads GP" + pin
                + ", which this board gives to " + _binding.ReservedBy[channel]);
        }
        EnsureUp();
        if (pin < 0) return;
        PrepareAnalogPad(_ioCtrl0 + Rp2350IoBank0Layout.GPIO_CTRL_STRIDE * (uint)pin,
            _pads0 + Rp2350PadsBank0Layout.GPIO_STRIDE * (uint)pin);
        if (_binding.TwinPads[channel] != 0u)
        {
            PrepareAnalogPad(_binding.TwinIoCtrl[channel], _binding.TwinPads[channel]);
        }
    }

    static void PrepareAnalogPad(uint ioCtrl, uint pads)
    {
        Mmio.Write32(ioCtrl, Rp2350IoBank0Layout.FUNCSEL_NULL);
        Mmio.Write32(pads, Rp2350PadsBank0Layout.GPIO0_OD);
    }

    public override void CloseChannel(int channel)
    {
    }

    /// <summary>One-shot conversion (the strata's read_once): select the mux input,
    /// START_ONCE, poll READY, read RESULT -- re-converting when CS.ERR flags the sample
    /// (the datasheet CAUTION: an error sample is undefined and must be discarded). A
    /// conversion whose READY never comes answers the seam's failure status, since RESULT
    /// still holds the conversion before it.</summary>
    public override int ReadValue(int channel)
    {
        if (!EnsureUp()) return ConversionFailed;
        uint select = ((uint)channel << (int)Rp2350AdcLayout.CS_AINSEL_LSB)
            | Rp2350AdcLayout.CS_EN | Rp2350AdcLayout.CS_TS_EN;
        for (int attempt = 0; attempt < 8; attempt++)
        {
            Mmio.Write32(_cs, select);
            Mmio.Write32(_cs, select | Rp2350AdcLayout.CS_START_ONCE);
            if (!WaitReady()) return ConversionFailed;
            if ((Mmio.Read32(_cs) & Rp2350AdcLayout.CS_ERR) == 0u)
            {
                return (int)(Mmio.Read32(_result) & Rp2350AdcLayout.RESULT_RESULT);
            }
        }
        return ConversionFailed;
    }

    bool WaitReady()
    {
        for (int spin = 0; spin < 100000; spin++)
        {
            if ((Mmio.Read32(_cs) & Rp2350AdcLayout.CS_READY) != 0u) return true;
        }
        return false;
    }

    int ChannelPin(int channel)
    {
        return channel >= 0 && channel < _binding.ChannelPins.Length ? _binding.ChannelPins[channel] : -1;
    }
}
