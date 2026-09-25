// The descriptor an nRF51 SPI master driver consumes: one binding's resolved values, exactly the
// constants a board's generated Bindings class carries.
namespace Lamella.Boards
{
    public sealed class Nrf51SpiBinding
    {
        /// <summary>The bound SPI instance's base address. It shares its peripheral ID -- and so
        /// this register block -- with the other serial personalities at the same base; ENABLE is
        /// what selects the SPI master.</summary>
        public readonly uint SpiBase;
        /// <summary>The PSELSCK routing value. On this part that is simply the PIN NUMBER: the
        /// register is a plain 32-bit pin field because the chip has one GPIO port. Latched only
        /// while the master is disabled.</summary>
        public readonly uint PselSck;
        /// <summary>The PSELMOSI routing value, same encoding.</summary>
        public readonly uint PselMosi;
        /// <summary>The PSELMISO routing value, same encoding.</summary>
        public readonly uint PselMiso;
        /// <summary>The SCK pin's PIN_CNF register address.</summary>
        public readonly uint PinCnfSckReg;
        /// <summary>The MOSI pin's PIN_CNF register address.</summary>
        public readonly uint PinCnfMosiReg;
        /// <summary>The MISO pin's PIN_CNF register address.</summary>
        public readonly uint PinCnfMisoReg;

        public Nrf51SpiBinding(uint spiBase, uint pselSck, uint pselMosi, uint pselMiso,
            uint pinCnfSckReg, uint pinCnfMosiReg, uint pinCnfMisoReg)
        {
            SpiBase = spiBase;
            PselSck = pselSck;
            PselMosi = pselMosi;
            PselMiso = pselMiso;
            PinCnfSckReg = pinCnfSckReg;
            PinCnfMosiReg = pinCnfMosiReg;
            PinCnfMisoReg = pinCnfMisoReg;
        }
    }
}
