// Lamella.Boards.St.NucleoH755ziQ -- the ST NUCLEO-H755ZI-Q (STM32H755ZI, Cortex-M7 + Cortex-M4).
using System.Device.Gpio;
using Lamella.Generated;
using Lamella.Hardware;

namespace Lamella.Boards.St
{
    public sealed class NucleoH755ziQ
    {
        /// <summary>The wire identity this board advertises (lamella_wire::board_model).</summary>
        public static readonly int BoardModel = NucleoH755ziQBindings.BOARD_MODEL;

        /// <summary>The green user LED LD1.</summary>
        public static readonly int Led1Pin =
            Stm32h7GpioDriver.LogicalPin(NucleoH755ziQBindings.LED0_PORT_BASE, NucleoH755ziQBindings.LED0_PIN);

        /// <summary>The blue user LED LD2 -- on a DIFFERENT port from the other two.</summary>
        public static readonly int Led2Pin =
            Stm32h7GpioDriver.LogicalPin(NucleoH755ziQBindings.LED1_PORT_BASE, NucleoH755ziQBindings.LED1_PIN);

        /// <summary>The red user LED LD3.</summary>
        public static readonly int Led3Pin =
            Stm32h7GpioDriver.LogicalPin(NucleoH755ziQBindings.LED2_PORT_BASE, NucleoH755ziQBindings.LED2_PIN);

        /// <summary>Binds this board's GPIO block to the driver table, so a program writes plain
        /// dotnet/iot -- <c>new GpioController()</c> -- and never names a Lamella type. Touching
        /// <see cref="NucleoH755ziQ"/> at all is what arms it, which is why a program constructs the board
        /// first.</summary>
        /// <remarks>A TYPE INITIALIZER rather than the instance constructor, for the reason
        /// <see cref="Lamella.Hardware.Buses.BindGpio"/> documents: the table refuses a second bind
        /// of the same kind rather than replacing it, and this class is instantiable and routinely
        /// constructed as a temporary. The language runs a type initializer once per program, so
        /// idempotence costs nothing and the table keeps its throw as a genuine-error detector.
        /// The bound value is a FACTORY, not a driver, so a program that never touches GPIO never
        /// constructs one.</remarks>
        static NucleoH755ziQ()
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
