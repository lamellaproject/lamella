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
    public Rp2350AdcDriver(Rp2350AdcBinding binding)
    {
        _binding = binding;
        _cs = binding.AdcBase + Rp2350AdcLayout.CS_OFF;
        _result = binding.AdcBase + Rp2350AdcLayout.RESULT_OFF;
        _clkAdcCtrl = Rp2350Instances.CLOCKS_BASE + Rp2350ClocksLayout.CLK_ADC_CTRL_OFF;
        _resetsClr = Rp2350Instances.RESETS_CLR_BASE + Rp2350ResetsLayout.RESET_OFF;
        _resetsDone = Rp2350Instances.RESETS_BASE + Rp2350ResetsLayout.RESET_DONE_OFF;
        _ioCtrl0 = Rp2350Instances.IO_BANK0_BASE + Rp2350IoBank0Layout.GPIO0_CTRL_OFF;
        _pads0 = Rp2350Instances.PADS_BANK0_BASE + Rp2350PadsBank0Layout.GPIO0_OFF;
        _channelCount = Rp2350AdcLayout.ChannelCount;
        _resolutionBits = (int)Rp2350AdcLayout.ResolutionBits;
        _minValue = (int)Rp2350AdcLayout.MinValue;
        _maxValue = (int)Rp2350AdcLayout.MaxValue;
    }

    void EnsureUp()
    {
        if (_up) return;

        uint auxPllUsb = Rp2350ClocksLayout.CLK_ADC_AUXSRC_PLL_USB << (int)Rp2350ClocksLayout.CLK_ADC_CTRL_AUXSRC_LSB;
        uint ctrl = Mmio.Read32(_clkAdcCtrl);
        bool running = (ctrl & Rp2350ClocksLayout.CLK_ADC_CTRL_ENABLED) != 0u;
        bool onPllUsb = (ctrl & Rp2350ClocksLayout.CLK_ADC_CTRL_AUXSRC) == auxPllUsb;
        if (!running || !onPllUsb)
        {
            Mmio.Write32(_clkAdcCtrl, ctrl & ~Rp2350ClocksLayout.CLK_ADC_CTRL_ENABLE);
            for (int spin = 0; spin < 100000; spin++)
            {
                if ((Mmio.Read32(_clkAdcCtrl) & Rp2350ClocksLayout.CLK_ADC_CTRL_ENABLED) == 0u) break;
            }
            Mmio.Write32(_clkAdcCtrl, auxPllUsb);
            Mmio.Write32(_clkAdcCtrl, auxPllUsb | Rp2350ClocksLayout.CLK_ADC_CTRL_ENABLE);
            for (int spin = 0; spin < 100000; spin++)
            {
                if ((Mmio.Read32(_clkAdcCtrl) & Rp2350ClocksLayout.CLK_ADC_CTRL_ENABLED) != 0u) break;
            }
        }

        Mmio.Write32(_resetsClr, _binding.ResetMask);
        for (int spin = 0; spin < 100000; spin++)
        {
            if ((Mmio.Read32(_resetsDone) & _binding.ResetMask) == _binding.ResetMask) break;
        }

        Mmio.Write32(_cs, Rp2350AdcLayout.CS_EN | Rp2350AdcLayout.CS_TS_EN);
        for (int spin = 0; spin < 100000; spin++)
        {
            if ((Mmio.Read32(_cs) & Rp2350AdcLayout.CS_READY) != 0u) break;
        }
        _up = true;
    }

    /// <summary>The board's reference rail in microvolts (the binding's board truth) --
    /// what a raw count converts against.</summary>
    public int ReferenceMicrovolts { get { return (int)_binding.ReferenceMicrovolts; } }

    /// <summary>The package's mux width (QFN-60: four pin channels + the temperature sensor).</summary>
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
    /// pulls off, no digital function). The temperature channel needs none -- its bias rides
    /// CS.TS_EN from init.</summary>
    public override void OpenChannel(int channel)
    {
        EnsureUp();
        int pin = ChannelPin(channel);
        if (pin < 0) return;
        Mmio.Write32(_ioCtrl0 + Rp2350IoBank0Layout.GPIO_CTRL_STRIDE * (uint)pin, Rp2350IoBank0Layout.FUNCSEL_NULL);
        Mmio.Write32(_pads0 + Rp2350PadsBank0Layout.GPIO_STRIDE * (uint)pin, Rp2350PadsBank0Layout.GPIO0_OD);
    }

    public override void CloseChannel(int channel)
    {
    }

    /// <summary>One-shot conversion (the strata's read_once): select the mux input,
    /// START_ONCE, poll READY, read RESULT -- re-converting when CS.ERR flags the sample
    /// (the datasheet CAUTION: an error sample is undefined and must be discarded).</summary>
    public override int ReadValue(int channel)
    {
        EnsureUp();
        uint select = ((uint)channel << (int)Rp2350AdcLayout.CS_AINSEL_LSB)
            | Rp2350AdcLayout.CS_EN | Rp2350AdcLayout.CS_TS_EN;
        for (int attempt = 0; attempt < 8; attempt++)
        {
            Mmio.Write32(_cs, select);
            Mmio.Write32(_cs, select | Rp2350AdcLayout.CS_START_ONCE);
            for (int spin = 0; spin < 100000; spin++)
            {
                if ((Mmio.Read32(_cs) & Rp2350AdcLayout.CS_READY) != 0u) break;
            }
            if ((Mmio.Read32(_cs) & Rp2350AdcLayout.CS_ERR) == 0u)
            {
                return (int)(Mmio.Read32(_result) & Rp2350AdcLayout.RESULT_RESULT);
            }
        }
        return ConversionFailed;
    }

    static int ChannelPin(int channel)
    {
        if (channel == 0) return Rp2350AdcLayout.Channel0_Pin;
        if (channel == 1) return Rp2350AdcLayout.Channel1_Pin;
        if (channel == 2) return Rp2350AdcLayout.Channel2_Pin;
        if (channel == 3) return Rp2350AdcLayout.Channel3_Pin;
        return -1;
    }
}
