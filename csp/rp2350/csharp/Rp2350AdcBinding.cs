// The descriptor an RP2350 SAR-ADC driver consumes: the converter
namespace Lamella.Boards
{
    public sealed class Rp2350AdcBinding
    {
        /// <summary>The ADC block's base address.</summary>
        public readonly uint AdcBase;
        /// <summary>The ADC's reset-release mask (the converter releases alone; its inputs
        /// are analogue pads, not IO-bank routes).</summary>
        public readonly uint ResetMask;
        /// <summary>The board's ADC_VREF/ADC_AVDD rail in microvolts -- what a raw count
        /// converts against.</summary>
        public readonly uint ReferenceMicrovolts;

        public Rp2350AdcBinding(uint adcBase, uint resetMask, uint referenceMicrovolts)
        {
            AdcBase = adcBase;
            ResetMask = resetMask;
            ReferenceMicrovolts = referenceMicrovolts;
        }
    }
}
