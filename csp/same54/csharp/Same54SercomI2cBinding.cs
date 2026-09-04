// The descriptor a SAM E54 SERCOM-I2C-master driver consumes: one binding's resolved values,
namespace Lamella.Boards
{
    public sealed class Same54SercomI2cBinding
    {
        /// <summary>The bound SERCOM instance's base address.</summary>
        public readonly uint SercomBase;
        /// <summary>The address of this instance's GCLK peripheral channel register, PCHCTRL[m],
        /// resolved from the instance's channel index. An ADDRESS rather than an index, because
        /// the index is only meaningful against the GCLK base and the array stride, and neither
        /// is a fact this driver should hold.</summary>
        public readonly uint GclkPchctrlReg;
        /// <summary>The value to store there: the generator selection with the channel-enable bit
        /// set. Selecting a generator without that bit leaves the peripheral unclocked while the
        /// register reads back the generator that was asked for.</summary>
        public readonly uint GclkPchctrlValue;
        /// <summary>The address of the MCLK APB mask register that gates this instance's bus
        /// clock. WHICH register differs per instance on this family.</summary>
        public readonly uint ApbMaskReg;
        /// <summary>This instance's bit within that mask register, as a mask.</summary>
        public readonly uint ApbMask;
        /// <summary>The resolved PORT PMUX byte address covering the SDA/SCL pin pair. One byte
        /// covers an even/odd pin pair, so an SDA/SCL pair on adjacent pins needs one store.</summary>
        public readonly uint PmuxReg;
        /// <summary>The composed PMUX byte: the mux function in both nibbles.</summary>
        public readonly uint PmuxPair;
        /// <summary>The resolved PORT PINCFG byte address of the SDA pin.</summary>
        public readonly uint PincfgSdaReg;
        /// <summary>The resolved PORT PINCFG byte address of the SCL pin.</summary>
        public readonly uint PincfgSclReg;
        /// <summary>The rate of the GCLK generator feeding this SERCOM's core clock, in Hz -- a
        /// PLAN fact rather than a chip fact. The driver derives BAUD from it and the caller's
        /// wire rate, so the divisor is never a constant in either the driver or the board class.
        /// </summary>
        public readonly uint CoreClockHz;

        public Same54SercomI2cBinding(uint sercomBase, uint gclkPchctrlReg, uint gclkPchctrlValue,
            uint apbMaskReg, uint apbMask, uint pmuxReg, uint pmuxPair, uint pincfgSdaReg,
            uint pincfgSclReg, uint coreClockHz)
        {
            SercomBase = sercomBase;
            GclkPchctrlReg = gclkPchctrlReg;
            GclkPchctrlValue = gclkPchctrlValue;
            ApbMaskReg = apbMaskReg;
            ApbMask = apbMask;
            PmuxReg = pmuxReg;
            PmuxPair = pmuxPair;
            PincfgSdaReg = pincfgSdaReg;
            PincfgSclReg = pincfgSclReg;
            CoreClockHz = coreClockHz;
        }
    }
}
