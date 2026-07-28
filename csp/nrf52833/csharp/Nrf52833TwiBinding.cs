// The descriptor an nRF52833 TWI (legacy polled I2C master) driver consumes: one binding's
namespace Lamella.Boards
{
    public sealed class Nrf52833TwiBinding
    {
        /// <summary>The bound TWI instance's base address (it shares peripheral ID 3 with
        /// the SPI/SPIM/SPIS/TWIM/TWIS personalities; ENABLE selects TWI).</summary>
        public readonly uint TwiBase;
        /// <summary>The PSEL.SCL routing value ((port &lt;&lt; 5) | pin, CONNECT 0) -- latched
        /// only while the peripheral is disabled.</summary>
        public readonly uint PselScl;
        /// <summary>The PSEL.SDA routing value, same encoding.</summary>
        public readonly uint PselSda;
        /// <summary>The SCL pin's resolved PIN_CNF register address (the Table-51 electrical
        /// config target).</summary>
        public readonly uint PinCnfSclReg;
        /// <summary>The SDA pin's resolved PIN_CNF register address.</summary>
        public readonly uint PinCnfSdaReg;

        public Nrf52833TwiBinding(uint twiBase, uint pselScl, uint pselSda,
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
