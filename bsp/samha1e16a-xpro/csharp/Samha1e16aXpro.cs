// Lamella.Boards.Microchip.Samha1e16aXpro -- the SAMHA1E16A Xplained Pro (ATSAMHA1E16A-XPRO).
using System.Device.Gpio;
using Lamella.Generated;
using Lamella.Hardware;

namespace Lamella.Boards.Microchip
{
    public sealed class Samha1e16aXpro
    {
        /// <summary>The wire identity this board advertises (lamella_wire::product_model).</summary>
        public static readonly int BoardModel = Samha1e16aXproBindings.BOARD_MODEL;

        /// <summary>The yellow user LED LED0 on PA17, ACTIVE LOW: it lights when driven
        /// <see cref="PinValue.Low"/>.</summary>
        public static readonly int LedPin =
            Samha1GpioDriver.LogicalPin(Samha1e16aXproBindings.LED0_PORT_BASE, Samha1e16aXproBindings.LED0_PIN);

        /// <summary>The SW0 user button on PA18, ACTIVE LOW: pressing it drives the line to ground,
        /// so a pressed button reads <see cref="PinValue.Low"/>.</summary>
        public static readonly int ButtonPin =
            Samha1GpioDriver.LogicalPin(Samha1e16aXproBindings.BUTTON0_PORT_BASE, Samha1e16aXproBindings.BUTTON0_PIN);

        /// <summary>The mode the user button wants.</summary>
        /// <remarks>THIS KIT'S GUIDE DOES NOT SAY WHETHER AN EXTERNAL PULL-UP IS FITTED, where the
        /// Curiosity Nano guides state outright that none is. <see cref="PinMode.InputPullUp"/> is
        /// nevertheless the right answer under BOTH readings: if the board fits no pull-up the
        /// internal one is required, and if it fits one the internal resistor pulls the same way and
        /// changes nothing.</remarks>
        public static readonly PinMode ButtonMode = PinMode.InputPullUp;

        /// <summary>Binds this board's GPIO block to the driver table, so a program writes plain
        /// dotnet/iot -- <c>new GpioController()</c> -- and never names a Lamella type.</summary>
        /// <remarks>A TYPE INITIALIZER rather than the instance constructor, for the reason
        /// <see cref="Lamella.Hardware.Buses.BindGpio"/> documents: the table refuses a second bind
        /// of the same kind rather than replacing it, and this class is instantiable and routinely
        /// constructed as a temporary. The bound value is a FACTORY, so a program that never
        /// touches GPIO never constructs a driver.</remarks>
        static Samha1e16aXpro()
        {
            Buses.BindGpio(new GpioDriverFactory(MakeGpio));
        }

        private static GpioDriver MakeGpio() { return new Samha1GpioDriver(); }

        /// <summary>The family PORT driver this board bound.</summary>
        /// <remarks>THE DRIVER SPANS BOTH PORT GROUPS AND THIS PACKAGE BONDS PADS IN ONLY ONE. A
        /// group-B pin number is addressable here and reaches no package pin, which is why the
        /// constants above name only pads this board's own guide names.</remarks>
        public GpioDriver CreateGpioDriver()
        {
            return Buses.ResolveGpio();
        }

        /// <summary>A GPIO controller over the SAM HA1's PORT block.</summary>
        public GpioController CreateGpioController()
        {
            return new GpioController();
        }
    }
}
