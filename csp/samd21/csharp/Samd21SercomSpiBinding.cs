// The descriptor a SAMD21 SERCOM-SPI master driver consumes: one binding's resolved values.
namespace Lamella.Boards
{
    public sealed class Samd21SercomSpiBinding
    {
        /// <summary>The bound SERCOM instance's base address.</summary>
        public readonly uint SercomBase;
        /// <summary>The composed GCLK.CLKCTRL word routing the instance's core clock
        /// (ID | generator | CLKEN), derived from the instance row and the board plan.</summary>
        public readonly uint GclkClkctrlValue;
        /// <summary>The instance's PM.APBCMASK gate bit, as a mask.</summary>
        public readonly uint ApbcMask;
        /// <summary>The PORT PMUX byte address covering the MOSI pin.</summary>
        public readonly uint PmuxMosiReg;
        /// <summary>The MOSI pin's nibble within that byte: 0 for an even pin, 4 for an odd one.</summary>
        public readonly uint PmuxMosiShift;
        /// <summary>The PORT PINCFG byte address of the MOSI pin.</summary>
        public readonly uint PincfgMosiReg;
        /// <summary>The PORT PMUX byte address covering the SCK pin.</summary>
        public readonly uint PmuxSckReg;
        /// <summary>The SCK pin's nibble within that byte.</summary>
        public readonly uint PmuxSckShift;
        /// <summary>The PORT PINCFG byte address of the SCK pin.</summary>
        public readonly uint PincfgSckReg;
        /// <summary>The PORT PMUX byte address covering the MISO pin.</summary>
        public readonly uint PmuxMisoReg;
        /// <summary>The MISO pin's nibble within that byte.</summary>
        public readonly uint PmuxMisoShift;
        /// <summary>The PORT PINCFG byte address of the MISO pin.</summary>
        public readonly uint PincfgMisoReg;
        /// <summary>The peripheral function the three pins select, as the PMUX nibble value.</summary>
        public readonly uint PmuxFunc;
        /// <summary>The CTRLA.DOPO value placing data out and SCK on the wired pads.</summary>
        public readonly uint Dopo;
        /// <summary>The CTRLA.DIPO value naming the pad data in arrives on.</summary>
        public readonly uint Dipo;
        /// <summary>The rate of the GCLK generator feeding this SERCOM's core clock, in Hz -- a
        /// PLAN fact. The driver derives BAUD from it and the requested clock, so the divisor is
        /// never a constant in either the driver or the board class.</summary>
        public readonly uint CoreClockHz;

        public Samd21SercomSpiBinding(uint sercomBase, uint gclkClkctrlValue, uint apbcMask,
            uint pmuxMosiReg, uint pmuxMosiShift, uint pincfgMosiReg,
            uint pmuxSckReg, uint pmuxSckShift, uint pincfgSckReg,
            uint pmuxMisoReg, uint pmuxMisoShift, uint pincfgMisoReg,
            uint pmuxFunc, uint dopo, uint dipo, uint coreClockHz)
        {
            SercomBase = sercomBase;
            GclkClkctrlValue = gclkClkctrlValue;
            ApbcMask = apbcMask;
            PmuxMosiReg = pmuxMosiReg;
            PmuxMosiShift = pmuxMosiShift;
            PincfgMosiReg = pincfgMosiReg;
            PmuxSckReg = pmuxSckReg;
            PmuxSckShift = pmuxSckShift;
            PincfgSckReg = pincfgSckReg;
            PmuxMisoReg = pmuxMisoReg;
            PmuxMisoShift = pmuxMisoShift;
            PincfgMisoReg = pincfgMisoReg;
            PmuxFunc = pmuxFunc;
            Dopo = dopo;
            Dipo = dipo;
            CoreClockHz = coreClockHz;
        }
    }
}
