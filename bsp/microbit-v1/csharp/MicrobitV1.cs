// Lamella.Boards.MicrobitV1 -- the BBC micro:bit v1 (nRF51822, Cortex-M0) board-support package.
using Lamella.Generated;
using Lamella.Hardware;

namespace Lamella.Boards
{
    public sealed class MicrobitV1
    {
        /// <summary>The bus-speed request the BSP configures the TWI for (standard mode, Hz).
        /// The chip's rate register is enumerated, so this is one of exactly three values.</summary>
        public const int I2cBusHz = 100000;

        public static readonly uint I2cFreqWord100k = Nrf51TwiLayout.FREQUENCY_K100;
        public static readonly uint I2cFreqWord250k = Nrf51TwiLayout.FREQUENCY_K250;
        public static readonly uint I2cFreqWord400k = Nrf51TwiLayout.FREQUENCY_K400;

        /// <summary>The `i2c` binding descriptor, lifted from the generated consts (one naming
        /// scheme -- the role's resolved facts in one construction).</summary>
        public Nrf51TwiBinding CreateI2cBinding()
        {
            return new Nrf51TwiBinding(
                MicrobitV1Bindings.I2C_TWI_BASE,
                MicrobitV1Bindings.I2C_PSEL_SCL,
                MicrobitV1Bindings.I2C_PSEL_SDA,
                MicrobitV1Bindings.I2C_PIN_CNF_SCL_REG,
                MicrobitV1Bindings.I2C_PIN_CNF_SDA_REG);
        }

        /// <summary>The board's I2C bus as the layer-1 driver, configured for
        /// <see cref="I2cBusHz"/>. The on-board motion sensor and the edge connector's pins 19
        /// and 20 are the same bus, so a scan here sees whatever is wired to the edge as well as
        /// what is soldered down.</summary>
        public I2cDriver CreateI2cBus()
        {
            Nrf51I2cDriver bus = new Nrf51I2cDriver(CreateI2cBinding());
            bus.Configure(I2cBusHz);
            return bus;
        }
    }
}
