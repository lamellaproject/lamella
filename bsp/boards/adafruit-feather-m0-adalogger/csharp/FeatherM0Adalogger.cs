// Lamella.Boards.Adafruit.FeatherM0Adalogger -- the Adafruit Feather M0 Adalogger (ATSAMD21G18A,
using System.Device.Gpio;
using Lamella.Generated;
using Lamella.Hardware;

namespace Lamella.Boards.Adafruit
{
    internal sealed class FeatherM0Adalogger
    {
        /// <summary>The wire identity this board advertises (lamella_wire::board_model).</summary>
        public static readonly int BoardModel = AdafruitFeatherM0AdaloggerBindings.BOARD_MODEL;

        /// <summary>The red LED beside the USB jack (Arduino D13) -- the board's blink target.</summary>
        public static readonly int LedPin =
            LogicalPin(AdafruitFeatherM0AdaloggerBindings.LED_PORT_BASE, AdafruitFeatherM0AdaloggerBindings.LED_PIN);

        /// <summary>The green LED beside the microSD socket. A pin a program drives, not a signal
        /// the socket raises: nothing lights it unless the program does.</summary>
        public static readonly int SdLedPin =
            LogicalPin(AdafruitFeatherM0AdaloggerBindings.LED_SD_PORT_BASE, AdafruitFeatherM0AdaloggerBindings.LED_SD_PIN);

        /// <summary>The microSD socket's card-detect line, ACTIVE LOW: a card present reads
        /// <see cref="PinValue.Low"/>, so the pin wants <see cref="PinMode.InputPullUp"/>.</summary>
        public static readonly int SdCardDetectPin = LogicalPin(
            AdafruitFeatherM0AdaloggerBindings.SD_CARD_DETECT_PORT_BASE,
            AdafruitFeatherM0AdaloggerBindings.SD_CARD_DETECT_PIN);

        /// <summary>The logical bus id of the microSD socket's SPI (MOSI PB10, SCK PB11 and MISO
        /// PA12, on SERCOM4), as <c>SpiConnectionSettings</c> names it.</summary>
        public const int SdSpiBusId = 0;

        /// <summary>The microSD socket's chip select (Arduino D4, PA08), as a pin number -- the pin
        /// <c>Storage.MountSdOverSpi</c> takes. It is the first of the SD bus's chip selects, so
        /// <c>SpiConnectionSettings.ChipSelectLine</c> 0 selects it too.</summary>
        public static readonly int SdChipSelectPin =
            LogicalPin(AdafruitFeatherM0AdaloggerBindings.SD_SPI_CS_PORT_BASE, AdafruitFeatherM0AdaloggerBindings.SD_SPI_CS_PIN);

        /// <summary>Binds this board's GPIO block to the driver table, so a program writes plain
        /// dotnet/iot -- <c>new GpioController()</c> -- and never names a Lamella type. Touching
        /// <see cref="FeatherM0Adalogger"/> at all is what arms it, and constructing
        /// <see cref="TargetBoard"/> touches it.</summary>
        /// <remarks>A TYPE INITIALIZER rather than the instance constructor, for the reason
        /// <see cref="Lamella.Hardware.Buses.BindGpio"/> documents: the table refuses a second bind
        /// of the same kind rather than replacing it, and this class is instantiable and routinely
        /// constructed as a temporary. The language runs a type initializer once per program, so
        /// idempotence costs nothing and the table keeps its throw as a genuine-error detector.
        /// The bound value is a FACTORY, not a driver, so a program that never touches GPIO never
        /// constructs one.</remarks>
        static FeatherM0Adalogger()
        {
            Buses.BindGpio(new GpioDriverFactory(MakeGpio));
            Buses.BindSpi(SdSpiBusId, new SpiDriverFactory(MakeSdSpi));
        }

        private static GpioDriver MakeGpio() { return new Samd21GpioDriver(); }

        private static SpiDriver MakeSdSpi()
        {
            return new Samd21SpiDriver(new Samd21SercomSpiBinding(
                AdafruitFeatherM0AdaloggerBindings.SD_SPI_SERCOM_BASE,
                AdafruitFeatherM0AdaloggerBindings.SD_SPI_GCLK_CLKCTRL_VALUE,
                AdafruitFeatherM0AdaloggerBindings.SD_SPI_APBC_MASK,
                AdafruitFeatherM0AdaloggerBindings.SD_SPI_PMUX_MOSI_REG,
                AdafruitFeatherM0AdaloggerBindings.SD_SPI_PMUX_MOSI_SHIFT,
                AdafruitFeatherM0AdaloggerBindings.SD_SPI_PINCFG_MOSI_REG,
                AdafruitFeatherM0AdaloggerBindings.SD_SPI_PMUX_SCK_REG,
                AdafruitFeatherM0AdaloggerBindings.SD_SPI_PMUX_SCK_SHIFT,
                AdafruitFeatherM0AdaloggerBindings.SD_SPI_PINCFG_SCK_REG,
                AdafruitFeatherM0AdaloggerBindings.SD_SPI_PMUX_MISO_REG,
                AdafruitFeatherM0AdaloggerBindings.SD_SPI_PMUX_MISO_SHIFT,
                AdafruitFeatherM0AdaloggerBindings.SD_SPI_PINCFG_MISO_REG,
                AdafruitFeatherM0AdaloggerBindings.SD_SPI_PMUX_FUNC,
                AdafruitFeatherM0AdaloggerBindings.SD_SPI_DOPO,
                AdafruitFeatherM0AdaloggerBindings.SD_SPI_DIPO,
                AdafruitFeatherM0AdaloggerBindings.SD_SPI_CORE_CLOCK_HZ,
                new int[] { (int)AdafruitFeatherM0AdaloggerBindings.SD_SPI_CHIP_SELECT0 }));
        }

        /// <summary>The family PORT driver this board bound, over every pin on the part.</summary>
        /// <remarks>THE SAME INSTANCE <see cref="GpioController"/> drives. One block has one
        /// driver, and handing out a second one over the same registers reads as working while the
        /// facade talks to the first -- see <see cref="Lamella.Hardware.Buses.ResolveSpi"/> for the
        /// full argument.</remarks>
        public GpioDriver CreateGpioDriver()
        {
            return Buses.ResolveGpio();
        }

        /// <summary>A GPIO controller over the SAMD21 PORT block.</summary>
        public GpioController CreateGpioController()
        {
            return new GpioController();
        }

        /// <summary>The driver's logical numbering: group * 32 + pin, with the group taken from the
        /// binding's port base. Kept here rather than in the driver because it converts a BOARD fact
        /// (which port a device sits on) into the family's pin space.</summary>
        static int LogicalPin(uint portBase, uint pin)
        {
            int group = portBase == Samd21Instances.PORTB_BASE ? 1 : 0;
            return group * 32 + (int)pin;
        }
    }
}
