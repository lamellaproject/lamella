// The descriptor an STM32L476 USART driver consumes: one binding's resolved values, exactly the
namespace Lamella.Boards
{
    /// <summary>The resolved mux facts for ONE bound pin: the port clock gate to open, and the
    /// two read-modify-writes that put the pin into alternate-function mode on its function
    /// number.</summary>
    public sealed class Stm32l476UsartPinMux
    {
        /// <summary>The RCC register gating this pin's port clock.</summary>
        public readonly uint PortRccEnReg;
        /// <summary>That port's clock-enable bit, as a mask.</summary>
        public readonly uint PortRccEnMask;
        /// <summary>The GPIO MODER register covering this pin.</summary>
        public readonly uint ModerReg;
        /// <summary>This pin's MODER field span, to clear before setting.</summary>
        public readonly uint ModerMask;
        /// <summary>The MODER value selecting alternate-function mode for this pin.</summary>
        public readonly uint ModerValue;
        /// <summary>The GPIO alternate-function register covering this pin -- AFRL for pins 0..7,
        /// AFRH for pins 8..15. Which one it is has already been decided.</summary>
        public readonly uint AfrReg;
        /// <summary>This pin's alternate-function nibble, to clear before setting.</summary>
        public readonly uint AfrMask;
        /// <summary>The alternate-function selection for this pin (a DATASHEET fact -- the AF
        /// number appears nowhere in the reference manual).</summary>
        public readonly uint AfrValue;

        public Stm32l476UsartPinMux(uint portRccEnReg, uint portRccEnMask, uint moderReg,
            uint moderMask, uint moderValue, uint afrReg, uint afrMask, uint afrValue)
        {
            PortRccEnReg = portRccEnReg;
            PortRccEnMask = portRccEnMask;
            ModerReg = moderReg;
            ModerMask = moderMask;
            ModerValue = moderValue;
            AfrReg = afrReg;
            AfrMask = afrMask;
            AfrValue = afrValue;
        }
    }

    public sealed class Stm32l476UsartBinding
    {
        /// <summary>The bound USART instance's base address.</summary>
        public readonly uint UsartBase;
        /// <summary>The RCC register gating the USART instance's clock.</summary>
        public readonly uint RccEnReg;
        /// <summary>The USART instance's clock-enable bit, as a mask.</summary>
        public readonly uint RccEnMask;
        /// <summary>The transmit pin's mux facts.</summary>
        public readonly Stm32l476UsartPinMux Tx;
        /// <summary>The receive pin's mux facts.</summary>
        public readonly Stm32l476UsartPinMux Rx;
        /// <summary>The BRR divisor for the binding's wire rate under the board's default clock
        /// plan (plan-derived, so it names its clock source at the board stratum).</summary>
        public readonly uint BaudDivisor;

        public Stm32l476UsartBinding(uint usartBase, uint rccEnReg, uint rccEnMask,
            Stm32l476UsartPinMux tx, Stm32l476UsartPinMux rx, uint baudDivisor)
        {
            UsartBase = usartBase;
            RccEnReg = rccEnReg;
            RccEnMask = rccEnMask;
            Tx = tx;
            Rx = rx;
            BaudDivisor = baudDivisor;
        }
    }
}
