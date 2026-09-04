// The descriptor a SAM E54 ADC driver consumes: one binding's resolved values, exactly the consts
namespace Lamella.Boards
{
    public sealed class Same54AdcBinding
    {
        /// <summary>The bound converter's base address.</summary>
        public readonly uint AdcBase;
        /// <summary>The address of this instance's GCLK peripheral channel register.</summary>
        public readonly uint GclkPchctrlReg;
        /// <summary>The value to store there: a generator selection with the channel enabled.</summary>
        public readonly uint GclkPchctrlValue;
        /// <summary>The MCLK APB mask register gating this instance's bus clock. WHICH register
        /// differs per instance on this family.</summary>
        public readonly uint ApbMaskReg;
        /// <summary>This instance's bit within that mask register, as a mask.</summary>
        public readonly uint ApbMask;
        /// <summary>The address of this converter's own CALIB register.</summary>
        public readonly uint CalibReg;
        /// <summary>Where the production calibration word is read from.</summary>
        public readonly uint NvmCalibArea;
        /// <summary>The bit position at which THIS converter's three calibration values start in
        /// that word. One number locates all three: they are contiguous and three bits apart.</summary>
        public readonly uint NvmCalibLsb;
        /// <summary>The PORT PMUX byte covering this pad.</summary>
        public readonly uint PmuxReg;
        /// <summary>The nibble of that byte belonging to this pad. A MASK rather than a whole byte,
        /// because the other nibble belongs to the neighbouring pad and must survive the store.</summary>
        public readonly uint PmuxMask;
        /// <summary>The analog function, already shifted into this pad's nibble.</summary>
        public readonly uint PmuxValue;
        /// <summary>The PORT PINCFG byte for this pad.</summary>
        public readonly uint PincfgReg;
        /// <summary>The converter's positive-input selection for this pad -- the numbered analog
        /// input the chip states this pad reaches on this converter.</summary>
        public readonly uint Muxpos;
        /// <summary>What a full-scale count means, in microvolts. A BOARD fact: it is the analog
        /// supply this design feeds the part, and the driver selects that supply as the reference,
        /// so the two must describe the same rail.</summary>
        public readonly uint ReferenceMicrovolts;

        public Same54AdcBinding(uint adcBase, uint gclkPchctrlReg, uint gclkPchctrlValue,
            uint apbMaskReg, uint apbMask, uint calibReg, uint nvmCalibArea, uint nvmCalibLsb,
            uint pmuxReg, uint pmuxMask, uint pmuxValue, uint pincfgReg, uint muxpos,
            uint referenceMicrovolts)
        {
            AdcBase = adcBase;
            GclkPchctrlReg = gclkPchctrlReg;
            GclkPchctrlValue = gclkPchctrlValue;
            ApbMaskReg = apbMaskReg;
            ApbMask = apbMask;
            CalibReg = calibReg;
            NvmCalibArea = nvmCalibArea;
            NvmCalibLsb = nvmCalibLsb;
            PmuxReg = pmuxReg;
            PmuxMask = pmuxMask;
            PmuxValue = pmuxValue;
            PincfgReg = pincfgReg;
            Muxpos = muxpos;
            ReferenceMicrovolts = referenceMicrovolts;
        }
    }
}
