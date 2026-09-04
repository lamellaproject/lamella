// Lamella.Boards.Arduino.PortentaH7 -- the Arduino Portenta H7 (STM32H747XI) on the ABX00042.
using System.Device.Gpio;
using Lamella.Generated;
using Lamella.Hardware;

namespace Lamella.Boards.Arduino
{
    public sealed class PortentaH7
    {
        /// <summary>The wire identity this board advertises (lamella_wire::board_model).</summary>
        public static readonly int BoardModel = PortentaH7Bindings.BOARD_MODEL;

        /// <summary>The red leg of DL1, ACTIVE LOW: it lights when driven <see cref="PinValue.Low"/>.</summary>
        public static readonly int LedRedPin =
            Stm32h7GpioDriver.LogicalPin(PortentaH7Bindings.LED_RED_PORT_BASE, PortentaH7Bindings.LED_RED_PIN);

        /// <summary>The green leg of DL1, ACTIVE LOW.</summary>
        public static readonly int LedGreenPin =
            Stm32h7GpioDriver.LogicalPin(PortentaH7Bindings.LED_GREEN_PORT_BASE, PortentaH7Bindings.LED_GREEN_PIN);

        /// <summary>The blue leg of DL1, ACTIVE LOW.</summary>
        public static readonly int LedBluePin =
            Stm32h7GpioDriver.LogicalPin(PortentaH7Bindings.LED_BLUE_PORT_BASE, PortentaH7Bindings.LED_BLUE_PIN);

        /// <summary>Binds this board's GPIO block to the driver table, so a program writes plain
        /// dotnet/iot -- <c>new GpioController()</c> -- and never names a Lamella type. Touching
        /// <see cref="PortentaH7"/> at all is what arms it, which is why a program constructs the board
        /// first.</summary>
        /// <remarks>A TYPE INITIALIZER rather than the instance constructor, for the reason
        /// <see cref="Lamella.Hardware.Buses.BindGpio"/> documents: the table refuses a second bind
        /// of the same kind rather than replacing it, and this class is instantiable and routinely
        /// constructed as a temporary. The language runs a type initializer once per program, so
        /// idempotence costs nothing and the table keeps its throw as a genuine-error detector.
        /// The bound value is a FACTORY, not a driver, so a program that never touches GPIO never
        /// constructs one.</remarks>
        static PortentaH7()
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
