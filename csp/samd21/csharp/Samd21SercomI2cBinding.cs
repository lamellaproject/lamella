// The descriptor a SAMD21 SERCOM-I2C-master driver consumes: one binding's resolved values,
namespace Lamella.Boards
{
    public sealed class Samd21SercomI2cBinding
    {
        /// <summary>The bound SERCOM instance's base address.</summary>
        public readonly uint SercomBase;
        /// <summary>The composed GCLK.CLKCTRL word routing the instance's core clock
        /// (ID | generator | CLKEN), derived from the instance row + the board plan.</summary>
        public readonly uint GclkClkctrlValue;
        /// <summary>The instance's PM.APBCMASK gate bit, as a mask.</summary>
        public readonly uint ApbcMask;
        /// <summary>The resolved PORT PMUX byte address covering the SDA/SCL pin pair.</summary>
        public readonly uint PmuxReg;
        /// <summary>The composed PMUX byte (the mux function in both nibbles).</summary>
        public readonly uint PmuxPair;
        /// <summary>The resolved PORT PINCFG byte address of the SDA pin.</summary>
        public readonly uint PincfgSdaReg;
        /// <summary>The resolved PORT PINCFG byte address of the SCL pin.</summary>
        public readonly uint PincfgSclReg;
        /// <summary>The rate of the GCLK generator feeding this SERCOM's core clock, in Hz --
        /// a PLAN fact. The driver derives BAUD from it and the caller's wire rate, so the
        /// divisor is never a constant in either the driver or the board class.</summary>
        public readonly uint CoreClockHz;

        public Samd21SercomI2cBinding(uint sercomBase, uint gclkClkctrlValue, uint apbcMask,
            uint pmuxReg, uint pmuxPair, uint pincfgSdaReg, uint pincfgSclReg, uint coreClockHz)
        {
            SercomBase = sercomBase;
            GclkClkctrlValue = gclkClkctrlValue;
            ApbcMask = apbcMask;
            PmuxReg = pmuxReg;
            PmuxPair = pmuxPair;
            PincfgSdaReg = pincfgSdaReg;
            PincfgSclReg = pincfgSclReg;
            CoreClockHz = coreClockHz;
        }
    }
}
