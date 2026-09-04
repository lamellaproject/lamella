// Lamella.Boards.Arduino.OptaWifi -- the Arduino Opta WiFi (AFX00002), an industrial PLC on an
using System.Device.Gpio;
using Lamella.Generated;
using Lamella.Hardware;

namespace Lamella.Boards.Arduino
{
    public sealed class OptaWifi
    {
        /// <summary>The wire identity this board advertises (lamella_wire::board_model).</summary>
        public static readonly int BoardModel = OptaWifiBindings.BOARD_MODEL;

        /// <summary>Relay output terminal 1, ACTIVE HIGH: driving it <see cref="PinValue.High"/>
        /// energizes the coil and closes the normally open contact.</summary>
        public static readonly int Relay1Pin =
            Stm32h7GpioDriver.LogicalPin(OptaWifiBindings.RELAY_1_PORT_BASE, OptaWifiBindings.RELAY_1_PIN);

        /// <summary>Relay output terminal 2. Its pin is LOWER than terminal 1's, which is the
        /// board's wiring and not a mistake here.</summary>
        public static readonly int Relay2Pin =
            Stm32h7GpioDriver.LogicalPin(OptaWifiBindings.RELAY_2_PORT_BASE, OptaWifiBindings.RELAY_2_PIN);

        /// <summary>Relay output terminal 3.</summary>
        public static readonly int Relay3Pin =
            Stm32h7GpioDriver.LogicalPin(OptaWifiBindings.RELAY_3_PORT_BASE, OptaWifiBindings.RELAY_3_PIN);

        /// <summary>Relay output terminal 4. Its pin is the LOWEST of the four.</summary>
        public static readonly int Relay4Pin =
            Stm32h7GpioDriver.LogicalPin(OptaWifiBindings.RELAY_4_PORT_BASE, OptaWifiBindings.RELAY_4_PIN);

        /// <summary>The user-programmable button, ACTIVE LOW behind an internal pull-up: pressed
        /// reads <see cref="PinValue.Low"/>, so the pin wants
        /// <see cref="PinMode.InputPullUp"/>.</summary>
        public static readonly int UserButtonPin =
            Stm32h7GpioDriver.LogicalPin(OptaWifiBindings.USER_BUTTON_PORT_BASE, OptaWifiBindings.USER_BUTTON_PIN);

        /// <summary>The indicator above the user button, fitted only on the WiFi variant.</summary>
        /// <remarks>The board file records no asserted level for this line, so there is no polarity
        /// constant to lift and none is invented here. The four FACEPLATE status LEDs are a
        /// different thing and have no pin at all -- they sit behind a SPI bus, which is why this
        /// class offers no property for them and a program reaching for one does not compile.</remarks>
        public static readonly int UserLedPin =
            Stm32h7GpioDriver.LogicalPin(OptaWifiBindings.USER_LED_PORT_BASE, OptaWifiBindings.USER_LED_PIN);

        /// <summary>Binds this board's GPIO block to the driver table, so a program writes plain
        /// dotnet/iot -- <c>new GpioController()</c> -- and never names a Lamella type. Touching
        /// <see cref="OptaWifi"/> at all is what arms it, which is why a program constructs the
        /// board first.</summary>
        /// <remarks>A TYPE INITIALIZER rather than the instance constructor, for the reason
        /// <see cref="Lamella.Hardware.Buses.BindGpio"/> documents: the table refuses a second bind
        /// of the same kind rather than replacing it, and this class is instantiable and routinely
        /// constructed as a temporary. The language runs a type initializer once per program, so
        /// idempotence costs nothing and the table keeps its throw as a genuine-error detector.
        /// The bound value is a FACTORY, not a driver, so a program that never touches GPIO never
        /// constructs one.</remarks>
        static OptaWifi()
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
