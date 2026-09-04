// Lamella.Boards.Adafruit.FeatherM0Wifi -- the Adafruit Feather M0 WiFi (ATSAMD21G18A, Cortex-M0+,
using System.Device.Gpio;
using Lamella.Generated;
using Lamella.Hardware;

namespace Lamella.Boards.Adafruit
{
    public sealed class FeatherM0Wifi
    {
        /// <summary>The wire identity this board advertises (lamella_wire::board_model).</summary>
        public static readonly int BoardModel = FeatherM0WifiBindings.BOARD_MODEL;

        /// <summary>The red LED beside the USB jack (Arduino D13) -- the board's blink target, and
        /// the only indicator visible without driving a protocol.</summary>
        public static readonly int LedPin =
            LogicalPin(FeatherM0WifiBindings.LED_PORT_BASE, FeatherM0WifiBindings.LED_PIN);

        /// <summary>The radio's chip-select line on the SPI bus it shares.</summary>
        public static readonly int WincChipSelectPin =
            LogicalPin(FeatherM0WifiBindings.WINC_SPI_CS_PORT_BASE, FeatherM0WifiBindings.WINC_SPI_CS_PIN);

        /// <summary>The radio's reset line, ACTIVE LOW: driving it <see cref="PinValue.Low"/> holds
        /// the WINC in reset.</summary>
        public static readonly int WincResetPin =
            LogicalPin(FeatherM0WifiBindings.WINC_RESET_N_PORT_BASE, FeatherM0WifiBindings.WINC_RESET_N_PIN);

        /// <summary>The radio's enable line, ACTIVE HIGH: driving it <see cref="PinValue.High"/>
        /// powers the WINC on. The opposite polarity from <see cref="WincResetPin"/>, which is a
        /// board fact rather than a convention and is why both are stated.</summary>
        public static readonly int WincChipEnablePin =
            LogicalPin(FeatherM0WifiBindings.WINC_CHIP_EN_PORT_BASE, FeatherM0WifiBindings.WINC_CHIP_EN_PIN);

        /// <summary>Binds this board's GPIO block to the driver table, so a program writes plain
        /// dotnet/iot -- <c>new GpioController()</c> -- and never names a Lamella type. Touching
        /// <see cref="FeatherM0Wifi"/> at all is what arms it, which is why a program constructs the
        /// board first.</summary>
        /// <remarks>A TYPE INITIALIZER rather than the instance constructor, for the reason
        /// <see cref="Lamella.Hardware.Buses.BindGpio"/> documents: the table refuses a second bind
        /// of the same kind rather than replacing it, and this class is instantiable and routinely
        /// constructed as a temporary. The language runs a type initializer once per program, so
        /// idempotence costs nothing and the table keeps its throw as a genuine-error detector.
        /// The bound value is a FACTORY, not a driver, so a program that never touches GPIO never
        /// constructs one.</remarks>
        static FeatherM0Wifi()
        {
            Buses.BindGpio(new GpioDriverFactory(MakeGpio));
        }

        private static GpioDriver MakeGpio() { return new Samd21GpioDriver(); }

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
