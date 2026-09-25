// The descriptor a SAMD21 ADC driver consumes: the converter's resolved clocking, and every analog
// pad a board wired to it.
namespace Lamella.Boards
{
    /// <summary>One analog pad a board wired to the converter: the channel it is, and where its
    /// multiplexer nibble and pin configuration byte are.</summary>
    public sealed class Samd21AdcPad
    {
        /// <summary>The converter channel this pad is: its AIN number, which is the multiplexer code
        /// that selects it.</summary>
        public readonly int Channel;
        /// <summary>The PORT PMUX byte covering the pad.</summary>
        public readonly uint PmuxReg;
        /// <summary>The pad's nibble within that byte: 0 for an even pad, 4 for an odd one.</summary>
        public readonly uint PmuxShift;
        /// <summary>The PORT PINCFG byte of the pad.</summary>
        public readonly uint PincfgReg;

        public Samd21AdcPad(int channel, uint pmuxReg, uint pmuxShift, uint pincfgReg)
        {
            Channel = channel;
            PmuxReg = pmuxReg;
            PmuxShift = pmuxShift;
            PincfgReg = pincfgReg;
        }
    }

    public sealed class Samd21AdcBinding
    {
        /// <summary>The converter's base address.</summary>
        public readonly uint AdcBase;
        /// <summary>The composed GCLK.CLKCTRL word routing the converter's clock (ID | generator |
        /// CLKEN), from the instance row and the board plan.</summary>
        public readonly uint GclkClkctrlValue;
        /// <summary>The converter's PM.APBCMASK gate bit, as a mask.</summary>
        public readonly uint ApbcMask;
        /// <summary>The CTRLB.PRESCALER value that brings the converter's clock within its limits
        /// under the board plan.</summary>
        public readonly uint Prescaler;
        /// <summary>The PMUX nibble value selecting the analog function on the pads.</summary>
        public readonly uint PmuxFunc;
        /// <summary>What a full-scale count means, in microvolts: the board's analog supply, which
        /// the driver measures against.</summary>
        public readonly uint ReferenceMicrovolts;

        private readonly Samd21AdcPad[] _pads;

        public Samd21AdcBinding(uint adcBase, uint gclkClkctrlValue, uint apbcMask, uint prescaler,
            uint pmuxFunc, uint referenceMicrovolts, Samd21AdcPad[] pads)
        {
            if ((object)pads == null)
            {
                throw new System.ArgumentNullException("pads");
            }
            AdcBase = adcBase;
            GclkClkctrlValue = gclkClkctrlValue;
            ApbcMask = apbcMask;
            Prescaler = prescaler;
            PmuxFunc = pmuxFunc;
            ReferenceMicrovolts = referenceMicrovolts;
            // A copy, so the table a driver was built over cannot change under it.
            _pads = new Samd21AdcPad[pads.Length];
            for (int i = 0; i < pads.Length; i++)
            {
                _pads[i] = pads[i];
            }
        }

        /// <summary>How many analog pads the board wired.</summary>
        public int PadCount { get { return _pads.Length; } }

        /// <summary>The wired pad at <paramref name="index"/>, from 0 to <see cref="PadCount"/> - 1.</summary>
        public Samd21AdcPad Pad(int index)
        {
            return _pads[index];
        }
    }
}
