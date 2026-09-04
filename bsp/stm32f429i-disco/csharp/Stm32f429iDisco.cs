// Lamella.Boards.St.Stm32f429iDisco -- the ST STM32F429I-DISC1 Discovery kit (STM32F429ZI on the
using System.Device.Gpio;
using Lamella.Generated;
using Lamella.Hardware;

namespace Lamella.Boards.St
{
    public sealed class Stm32f429iDisco
    {
        /// <summary>The wire identity this board advertises (lamella_wire::board_model).</summary>
        public static readonly int BoardModel = Stm32f429iDiscoBindings.BOARD_MODEL;

        /// <summary>The green user LED LD3, ACTIVE HIGH.</summary>
        public static readonly int LedGreenPin =
            Stm32f42xGpioDriver.LogicalPin(
                Stm32f429iDiscoBindings.LED_GREEN_PORT_BASE, Stm32f429iDiscoBindings.LED_GREEN_PIN);

        /// <summary>The red user LED LD4, ACTIVE HIGH.</summary>
        public static readonly int LedRedPin =
            Stm32f42xGpioDriver.LogicalPin(
                Stm32f429iDiscoBindings.LED_RED_PORT_BASE, Stm32f429iDiscoBindings.LED_RED_PIN);

        /// <summary>The B1 USER button, ACTIVE HIGH: the pin rests LOW and reads HIGH while the
        /// button is held. It reaches PA0 through SB2, whose factory position UM1670 does not
        /// state.</summary>
        public static readonly int ButtonPin =
            Stm32f42xGpioDriver.LogicalPin(
                Stm32f429iDiscoBindings.BUTTON_PORT_BASE, Stm32f429iDiscoBindings.BUTTON_PIN);

        /// <summary>Binds this board's GPIO block to the driver table, so a program writes plain
        /// dotnet/iot -- <c>new GpioController()</c> -- and never names a Lamella type. Touching
        /// <see cref="Stm32f429iDisco"/> at all is what arms it, which is why a program constructs
        /// the board first.</summary>
        /// <remarks>A TYPE INITIALIZER rather than the instance constructor, for the reason
        /// <see cref="Lamella.Hardware.Buses.BindGpio"/> documents: the table refuses a second bind
        /// of the same kind rather than replacing it, and this class is instantiable and routinely
        /// constructed as a temporary. The language runs a type initializer once per program, so
        /// idempotence costs nothing and the table keeps its throw as a genuine-error detector.
        /// The bound value is a FACTORY, not a driver, so a program that never touches GPIO never
        /// constructs one.</remarks>
        static Stm32f429iDisco()
        {
            Buses.BindGpio(new GpioDriverFactory(MakeGpio));
        }

        private static GpioDriver MakeGpio() { return new Stm32f42xGpioDriver(); }

        /// <summary>The family GPIO driver this board bound.</summary>
        /// <remarks>THE SAME INSTANCE <see cref="GpioController"/> drives. One block has one
        /// driver, and handing out a second one over the same registers reads as working while the
        /// facade talks to the first -- see <see cref="Lamella.Hardware.Buses.ResolveSpi"/> for the
        /// full argument.</remarks>
        public GpioDriver CreateGpioDriver()
        {
            return Buses.ResolveGpio();
        }

        /// <summary>A GPIO controller over the STM32F42x port block.</summary>
        public GpioController CreateGpioController()
        {
            return new GpioController();
        }

    }
}
