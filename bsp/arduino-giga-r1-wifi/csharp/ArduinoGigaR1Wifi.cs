// Lamella.Boards.Arduino.ArduinoGigaR1Wifi -- the Arduino GIGA R1 WiFi (STM32H747XI). Board truth
using System.Device.Gpio;
using Lamella.Generated;
using Lamella.Hardware;

namespace Lamella.Boards.Arduino
{
    public sealed class ArduinoGigaR1Wifi
    {
        /// <summary>The wire identity this board advertises (lamella_wire::board_model).</summary>
        public static readonly int BoardModel = ArduinoGigaR1WifiBindings.BOARD_MODEL;

        /// <summary>The red leg of the user RGB LED.</summary>
        public static readonly int LedRedPin =
            Stm32h7GpioDriver.LogicalPin(ArduinoGigaR1WifiBindings.LED_RED_PORT_BASE, ArduinoGigaR1WifiBindings.LED_RED_PIN);

        /// <summary>The green leg -- on a different port from the red.</summary>
        public static readonly int LedGreenPin =
            Stm32h7GpioDriver.LogicalPin(ArduinoGigaR1WifiBindings.LED_GREEN_PORT_BASE, ArduinoGigaR1WifiBindings.LED_GREEN_PIN);

        /// <summary>The blue leg -- on a third port again.</summary>
        public static readonly int LedBluePin =
            Stm32h7GpioDriver.LogicalPin(ArduinoGigaR1WifiBindings.LED_BLUE_PORT_BASE, ArduinoGigaR1WifiBindings.LED_BLUE_PIN);

        /// <summary>The user button.</summary>
        public static readonly int ButtonPin =
            Stm32h7GpioDriver.LogicalPin(ArduinoGigaR1WifiBindings.BUTTON_PORT_BASE, ArduinoGigaR1WifiBindings.BUTTON_PIN);

        /// <summary>Binds this board's GPIO block to the driver table, so a program writes plain
        /// dotnet/iot -- <c>new GpioController()</c> -- and never names a Lamella type. Touching
        /// <see cref="ArduinoGigaR1Wifi"/> at all is what arms it, which is why a program constructs the board
        /// first.</summary>
        /// <remarks>A TYPE INITIALIZER rather than the instance constructor, for the reason
        /// <see cref="Lamella.Hardware.Buses.BindGpio"/> documents: the table refuses a second bind
        /// of the same kind rather than replacing it, and this class is instantiable and routinely
        /// constructed as a temporary. The language runs a type initializer once per program, so
        /// idempotence costs nothing and the table keeps its throw as a genuine-error detector.
        /// The bound value is a FACTORY, not a driver, so a program that never touches GPIO never
        /// constructs one.</remarks>
        static ArduinoGigaR1Wifi()
        {
            Buses.BindGpio(new GpioDriverFactory(MakeGpio));
        }

        private static GpioDriver MakeGpio() { return new Stm32h7GpioDriver(); }

        /// <summary>The family GPIO driver this board bound, over PA0..PK15.</summary>
        /// <remarks>THE SAME INSTANCE <see cref="GpioController"/> drives. One block has one
        /// driver, and handing out a second one over the same registers reads as working while the
        /// facade talks to the first -- see <see cref="Lamella.Hardware.Buses.ResolveSpi"/> for the
        /// full argument.</remarks>
        public GpioDriver CreateGpioDriver()
        {
            return Buses.ResolveGpio();
        }

        /// <summary>A GPIO controller over the STM32H7 port block.</summary>
        public GpioController CreateGpioController()
        {
            return new GpioController();
        }

    }
}
