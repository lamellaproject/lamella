// The descriptor a SAMD21 SERCOM-USART driver consumes: one binding's resolved values, exactly
namespace Lamella.Boards
{
    public sealed class Samd21SercomUsartBinding
    {
        /// <summary>The bound SERCOM instance's base address.</summary>
        public readonly uint SercomBase;
        /// <summary>The composed GCLK.CLKCTRL word routing the instance's core clock
        /// (ID | generator | CLKEN), derived from the instance row + the board plan.</summary>
        public readonly uint GclkClkctrlValue;
        /// <summary>The instance's PM.APBCMASK gate bit, as a mask.</summary>
        public readonly uint ApbcMask;
        /// <summary>The resolved PORT PMUX byte address covering the TX/RX pin pair.</summary>
        public readonly uint PmuxReg;
        /// <summary>The composed PMUX byte (the mux function in both nibbles).</summary>
        public readonly uint PmuxPair;
        /// <summary>The resolved PORT PINCFG byte address of the TX pin.</summary>
        public readonly uint PincfgTxReg;
        /// <summary>The resolved PORT PINCFG byte address of the RX pin.</summary>
        public readonly uint PincfgRxReg;
        /// <summary>The CTRLA.TXPO pad-routing value for the TX pin.</summary>
        public readonly uint Txpo;
        /// <summary>The CTRLA.RXPO pad-routing value for the RX pin.</summary>
        public readonly uint Rxpo;
        /// <summary>The BAUD divisor for the binding's wire rate under the board's default
        /// clock plan (plan-derived).</summary>
        public readonly uint BaudDivisor;

        public Samd21SercomUsartBinding(uint sercomBase, uint gclkClkctrlValue, uint apbcMask,
            uint pmuxReg, uint pmuxPair, uint pincfgTxReg, uint pincfgRxReg,
            uint txpo, uint rxpo, uint baudDivisor)
        {
            SercomBase = sercomBase;
            GclkClkctrlValue = gclkClkctrlValue;
            ApbcMask = apbcMask;
            PmuxReg = pmuxReg;
            PmuxPair = pmuxPair;
            PincfgTxReg = pincfgTxReg;
            PincfgRxReg = pincfgRxReg;
            Txpo = txpo;
            Rxpo = rxpo;
            BaudDivisor = baudDivisor;
        }
    }
}
