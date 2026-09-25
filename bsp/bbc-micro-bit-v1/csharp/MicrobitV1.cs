// Lamella.Boards.Bbc.MicroBitV1 -- the BBC micro:bit v1 (nRF51822, Cortex-M0) board-support package.
using System.Device.Gpio;
using Lamella.Generated;
using Lamella.Hardware;

namespace Lamella.Boards.Bbc
{
    public sealed class MicroBitV1
    {
        /// <summary>Binds this board's GPIO port to the driver table, so a program writes plain
        /// dotnet/iot -- <c>new GpioController()</c> -- and never names a Lamella type. Touching
        /// <see cref="MicroBitV1"/> at all is what arms it, which is why a program constructs the
        /// board first.</summary>
        /// <remarks>A TYPE INITIALIZER rather than the instance constructor, for the reason
        /// <see cref="Lamella.Hardware.Buses.BindGpio"/> documents: the table refuses a second bind
        /// of the same kind rather than replacing it, and this class is instantiable and routinely
        /// constructed as a temporary. The language runs a type initializer once per program, so
        /// idempotence costs nothing and the table keeps its throw as a genuine-error detector.
        /// The bound value is a FACTORY, not a driver, so a program that never touches GPIO never
        /// constructs one.</remarks>
        static MicroBitV1()
        {
            Buses.BindGpio(new GpioDriverFactory(MakeGpio));
            Buses.BindSpi(EdgeSpiBusId, new SpiDriverFactory(MakeSpi));
        }

        /// <summary>The logical bus id of the edge connector's SPI (SCK P0.23, MISO P0.22, MOSI
        /// P0.21), as <c>SpiConnectionSettings</c> names it.</summary>
        public const int EdgeSpiBusId = 0;

        private static SpiDriver MakeSpi() { return new Nrf51SpiDriver(SpiBinding()); }

        /// <summary>The `spi` binding descriptor -- the edge connector's SPI -- lifted from the
        /// generated constants.</summary>
        public Nrf51SpiBinding CreateSpiBinding() { return SpiBinding(); }

        private static Nrf51SpiBinding SpiBinding()
        {
            return new Nrf51SpiBinding(
                BbcMicroBitV1Bindings.SPI_SPI_BASE,
                BbcMicroBitV1Bindings.SPI_PSEL_SCK,
                BbcMicroBitV1Bindings.SPI_PSEL_MOSI,
                BbcMicroBitV1Bindings.SPI_PSEL_MISO,
                BbcMicroBitV1Bindings.SPI_PIN_CNF_SCK_REG,
                BbcMicroBitV1Bindings.SPI_PIN_CNF_MOSI_REG,
                BbcMicroBitV1Bindings.SPI_PIN_CNF_MISO_REG);
        }

        /// <summary>The edge connector's SPI as the layer-1 driver the board's table binds for
        /// <see cref="EdgeSpiBusId"/>, not yet configured.</summary>
        /// <remarks>THE SAME INSTANCE <c>SpiDevice.Create</c> uses for that bus, for the reason
        /// <see cref="Lamella.Hardware.Buses.ResolveSpi"/> gives.</remarks>
        public SpiDriver CreateSpiDriver()
        {
            return Buses.ResolveSpi(EdgeSpiBusId);
        }

        // P0.24 and P0.25 carry this board's serial link to the on-board interface chip, so the
        // board reserves them: reconfiguring either would cut the connection to the host. Which
        // pins those are is board truth, which is why the driver takes them rather than knowing.
        private static GpioDriver MakeGpio() { return new Nrf51GpioDriver((1u << 24) | (1u << 25)); }

        /// <summary>The GPIO port, each pin numbered by its P0.n index -- the display's row 1 is
        /// pin 13.</summary>
        /// <remarks>THE SAME INSTANCE <see cref="GpioController"/> drives, resolved through the
        /// board's table rather than constructed here. One block has one driver, and handing out a
        /// second one over the same registers reads as working while the facade talks to the
        /// first -- see <see cref="Lamella.Hardware.Buses.ResolveSpi"/> for the full argument.</remarks>
        public GpioDriver CreateGpioDriver()
        {
            return Buses.ResolveGpio();
        }

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
                BbcMicroBitV1Bindings.I2C_TWI_BASE,
                BbcMicroBitV1Bindings.I2C_PSEL_SCL,
                BbcMicroBitV1Bindings.I2C_PSEL_SDA,
                BbcMicroBitV1Bindings.I2C_PIN_CNF_SCL_REG,
                BbcMicroBitV1Bindings.I2C_PIN_CNF_SDA_REG);
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
