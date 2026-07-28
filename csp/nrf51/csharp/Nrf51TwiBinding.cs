// The descriptor an nRF51 TWI (polled I2C master) driver consumes: one binding's resolved
namespace Lamella.Boards
{
    public sealed class Nrf51TwiBinding
    {
        /// <summary>The bound TWI instance's base address. It shares its peripheral ID -- and
        /// therefore this whole register block -- with the serial-peripheral personalities at
        /// the same base; ENABLE is what selects TWI.</summary>
        public readonly uint TwiBase;
        /// <summary>The PSELSCL routing value. On this part that is simply the PIN NUMBER: the
        /// register is a plain 32-bit pin field because the chip has one GPIO port, so there is
        /// no port bit to compose in and no connect bit to clear. Latched only while the
        /// peripheral is disabled.</summary>
        public readonly uint PselScl;
        /// <summary>The PSELSDA routing value, same encoding.</summary>
        public readonly uint PselSda;
        /// <summary>The SCL pin's resolved PIN_CNF register address (the electrical-config
        /// target the manual's TWI GPIO table names).</summary>
        public readonly uint PinCnfSclReg;
        /// <summary>The SDA pin's resolved PIN_CNF register address.</summary>
        public readonly uint PinCnfSdaReg;

        public Nrf51TwiBinding(uint twiBase, uint pselScl, uint pselSda,
            uint pinCnfSclReg, uint pinCnfSdaReg)
        {
            TwiBase = twiBase;
            PselScl = pselScl;
            PselSda = pselSda;
            PinCnfSclReg = pinCnfSclReg;
            PinCnfSdaReg = pinCnfSdaReg;
        }
    }
}
