// The descriptor an nRF52833 SPI (legacy polled master) driver consumes: one binding's resolved
// values, exactly the constants a board's generated Bindings class carries.
namespace Lamella.Boards
{
    public sealed class Nrf52833SpiBinding
    {
        /// <summary>The bound SPI instance's base address. It shares its peripheral ID -- and so
        /// this register block -- with the other serial personalities at the same base; ENABLE is
        /// what selects the SPI master.</summary>
        public readonly uint SpiBase;
        /// <summary>The PSEL.SCK routing value: the pin in bits 4:0, its port in bit 5, and the
        /// connect bit 31 clear. Latched only while the master is disabled.</summary>
        public readonly uint PselSck;
        /// <summary>The PSEL.MOSI routing value, same encoding.</summary>
        public readonly uint PselMosi;
        /// <summary>The PSEL.MISO routing value, same encoding.</summary>
        public readonly uint PselMiso;
        /// <summary>The SCK pin's PIN_CNF register address.</summary>
        public readonly uint PinCnfSckReg;
        /// <summary>The MOSI pin's PIN_CNF register address.</summary>
        public readonly uint PinCnfMosiReg;
        /// <summary>The MISO pin's PIN_CNF register address.</summary>
        public readonly uint PinCnfMisoReg;

        public Nrf52833SpiBinding(uint spiBase, uint pselSck, uint pselMosi, uint pselMiso,
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
