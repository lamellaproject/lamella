// The descriptor an STM32L476 USART driver consumes: one binding's resolved values, exactly the
namespace Lamella.Boards
{
    public sealed class Stm32l476UsartBinding
    {
        /// <summary>The bound USART instance's base address.</summary>
        public readonly uint UsartBase;
        /// <summary>The RCC register gating the USART instance's clock.</summary>
        public readonly uint RccEnReg;
        /// <summary>The USART instance's clock-enable bit, as a mask.</summary>
        public readonly uint RccEnMask;
        /// <summary>The RCC register gating the port clock of the TX/RX pins.</summary>
        public readonly uint PortRccEnReg;
        /// <summary>The pin port's clock-enable bit, as a mask.</summary>
        public readonly uint PortRccEnMask;
        /// <summary>The GPIO MODER register covering the TX/RX pin pair.</summary>
        public readonly uint ModerReg;
        /// <summary>The MODER field span of that pin pair, to clear before setting.</summary>
        public readonly uint ModerMask;
        /// <summary>The MODER value selecting alternate-function mode for both pins.</summary>
        public readonly uint ModerValue;
        /// <summary>The GPIO alternate-function register covering the pin pair.</summary>
        public readonly uint AfrReg;
        /// <summary>The alternate-function field span of that pin pair.</summary>
        public readonly uint AfrMask;
        /// <summary>The alternate-function selection for both pins (a DATASHEET fact -- the AF
        /// number appears nowhere in the reference manual).</summary>
        public readonly uint AfrValue;
        /// <summary>The BRR divisor for the binding's wire rate under the board's default clock
        /// plan (plan-derived, so it names its clock source at the board stratum).</summary>
        public readonly uint BaudDivisor;

        public Stm32l476UsartBinding(uint usartBase, uint rccEnReg, uint rccEnMask,
            uint portRccEnReg, uint portRccEnMask, uint moderReg, uint moderMask, uint moderValue,
            uint afrReg, uint afrMask, uint afrValue, uint baudDivisor)
        {
            UsartBase = usartBase;
            RccEnReg = rccEnReg;
            RccEnMask = rccEnMask;
            PortRccEnReg = portRccEnReg;
            PortRccEnMask = portRccEnMask;
            ModerReg = moderReg;
            ModerMask = moderMask;
            ModerValue = moderValue;
            AfrReg = afrReg;
            AfrMask = afrMask;
            AfrValue = afrValue;
            BaudDivisor = baudDivisor;
        }
    }
}
