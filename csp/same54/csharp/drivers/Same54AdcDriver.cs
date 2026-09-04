// A Lamella.Hardware.AdcDriver for the Microchip SAM E54's ADC, over Lamella.Hardware.Mmio -- ONE
using Lamella.Boards;
using Lamella.Generated;
using Lamella.Hardware;

public sealed class Same54AdcDriver : AdcDriver
{
    const int WaitBound = 100000;

    readonly Same54AdcBinding _binding;
    readonly uint _ctrla;
    readonly uint _inputctrl;
    readonly uint _ctrlb;
    readonly uint _refctrl;
    readonly uint _sampctrl;
    readonly uint _swtrig;
    readonly uint _intflag;
    readonly uint _syncbusy;
    readonly uint _result;

    public Same54AdcDriver(Same54AdcBinding binding)
    {
        _binding = binding;
        uint block = binding.AdcBase;
        _ctrla = block + Same54AdcLayout.CTRLA_OFF;
        _inputctrl = block + Same54AdcLayout.INPUTCTRL_OFF;
        _ctrlb = block + Same54AdcLayout.CTRLB_OFF;
        _refctrl = block + Same54AdcLayout.REFCTRL_OFF;
        _sampctrl = block + Same54AdcLayout.SAMPCTRL_OFF;
        _swtrig = block + Same54AdcLayout.SWTRIG_OFF;
        _intflag = block + Same54AdcLayout.INTFLAG_OFF;
        _syncbusy = block + Same54AdcLayout.SYNCBUSY_OFF;
        _result = block + Same54AdcLayout.RESULT_OFF;
        Configure();
    }

    /// <summary>The pad this driver was bound to, and only that one. See the header.</summary>
    public override int ChannelCount { get { return 1; } }

    /// <summary>Twelve bits, the resolution this driver selects.</summary>
    public override int ResolutionInBits { get { return 12; } }

    public override int MinValue { get { return 0; } }

    public override int MaxValue { get { return 4095; } }

    /// <summary>What a full-scale count means, in microvolts -- the board's analog supply, which
    /// this driver selects as the converter's reference.</summary>
    public uint ReferenceMicrovolts { get { return _binding.ReferenceMicrovolts; } }

    /// <summary>Single-ended only. The part can do differential, but a differential reading needs a
    /// SECOND pad muxed to the analog function and a board binding that names it, and this
    /// descriptor carries one pad. Claiming the mode without the pin would convert against an
    /// internally grounded input and return a plausible number.</summary>
    public override bool IsChannelModeSupported(AdcChannelMode mode)
    {
        return mode == AdcChannelMode.SingleEnded;
    }

    public override void SetChannelMode(AdcChannelMode mode)
    {
        if (mode != AdcChannelMode.SingleEnded)
        {
            throw new System.NotSupportedException();
        }
    }

    /// <summary>Nothing to do: the pad was muxed and the converter enabled at construction, and
    /// this driver has exactly one channel to open.</summary>
    public override void OpenChannel(int channel)
    {
        if (channel != 0)
        {
            throw new System.ArgumentOutOfRangeException();
        }
    }

    public override void CloseChannel(int channel) { }

    /// <summary>Starts a conversion and returns it. Blocking, and bounded.</summary>
    public override int ReadValue(int channel)
    {
        if (channel != 0)
        {
            throw new System.ArgumentOutOfRangeException();
        }
        Mmio.Write8(_intflag, (byte)Same54AdcLayout.INTFLAG_RESRDY);
        Mmio.Write8(_swtrig, (byte)Same54AdcLayout.SWTRIG_START);
        WaitSync(Same54AdcLayout.SYNCBUSY_SWTRIG);
        for (int spin = 0; spin < WaitBound; spin++)
        {
            if ((Mmio.Read8(_intflag) & Same54AdcLayout.INTFLAG_RESRDY) != 0)
            {
                return (int)Mmio.Read16(_result);
            }
        }
        return -1;
    }

    void Configure()
    {
        Mmio.Write32(_binding.ApbMaskReg, Mmio.Read32(_binding.ApbMaskReg) | _binding.ApbMask);
        Mmio.Write32(_binding.GclkPchctrlReg, _binding.GclkPchctrlValue);

        uint pmux = Mmio.Read8(_binding.PmuxReg);
        uint kept = pmux & ~_binding.PmuxMask;
        Mmio.Write8(_binding.PmuxReg, (byte)(kept | _binding.PmuxValue));
        Mmio.Write8(_binding.PincfgReg, (byte)Same54PortLayout.PINCFG0_PMUXEN);

        Mmio.Write16(_ctrla, (ushort)Same54AdcLayout.CTRLA_SWRST);
        WaitSync(Same54AdcLayout.SYNCBUSY_SWRST);

        LoadCalibration();

        Mmio.Write8(_refctrl, (byte)Same54AdcLayout.REFSEL_INTVCC1);
        WaitSync(Same54AdcLayout.SYNCBUSY_REFCTRL);
        Mmio.Write16(_ctrlb, (ushort)(Same54AdcLayout.RESSEL_12BIT
            << (int)Same54AdcLayout.CTRLB_RESSEL_LSB));
        WaitSync(Same54AdcLayout.SYNCBUSY_CTRLB);
        Mmio.Write8(_sampctrl, 32);
        WaitSync(Same54AdcLayout.SYNCBUSY_SAMPCTRL);
        Mmio.Write16(_inputctrl, (ushort)_binding.Muxpos);
        WaitSync(Same54AdcLayout.SYNCBUSY_INPUTCTRL);

        uint control = Mmio.Read16(_ctrla);
        Mmio.Write16(_ctrla, (ushort)(control | Same54AdcLayout.CTRLA_ENABLE));
        WaitSync(Same54AdcLayout.SYNCBUSY_ENABLE);
    }

    /// <summary>Copies this converter's three production calibration values out of the NVM software
    /// calibration area into CALIB, REPACKED -- see the header for why the order changes.</summary>
    void LoadCalibration()
    {
        uint word = Mmio.Read32(_binding.NvmCalibArea) >> (int)_binding.NvmCalibLsb;
        uint biascomp = word & 7u;
        uint biasrefbuf = (word >> (int)Same54AdcLayout.NVM_BIASREFBUF_OFFSET) & 7u;
        uint biasr2r = (word >> (int)Same54AdcLayout.NVM_BIASR2R_OFFSET) & 7u;
        uint calib = (biascomp << (int)Same54AdcLayout.CALIB_BIASCOMP_LSB)
            | (biasr2r << (int)Same54AdcLayout.CALIB_BIASR2R_LSB)
            | (biasrefbuf << (int)Same54AdcLayout.CALIB_BIASREFBUF_LSB);
        Mmio.Write16(_binding.CalibReg, (ushort)calib);
    }

    void WaitSync(uint bits)
    {
        for (int spin = 0; spin < WaitBound; spin++)
        {
            if ((Mmio.Read32(_syncbusy) & bits) == 0)
            {
                return;
            }
        }
    }
}
