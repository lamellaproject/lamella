// Lamella.Boards.Microchip.Samha1g16aXpro -- the SAM HA1G16A Xplained Pro (ATSAMHA1G16A-XPRO).
using System.Device.Gpio;
using Lamella.Generated;
using Lamella.Hardware;

namespace Lamella.Boards.Microchip
{
    public sealed class Samha1g16aXpro
    {
        /// <summary>The wire identity this board advertises (lamella_wire::product_model).</summary>
        public static readonly int BoardModel = Samha1g16aXproBindings.BOARD_MODEL;

        /// <summary>The yellow user LED LED0 on PA00, ACTIVE LOW: it lights when driven
        /// <see cref="PinValue.Low"/>.</summary>
        /// <remarks>THIS PAD IS SHARED WITH THE EXT1 HEADER, which the kit guide states in its own
        /// comment column, so a program driving the LED is also driving whatever an extension board
        /// presents on that line. It is additionally the part's XIN32 pad -- this board fits no
        /// 32.768 kHz crystal, so nothing is in contention here, but a board that fitted one could
        /// not also have this LED.</remarks>
        public static readonly int LedPin =
            Samha1GpioDriver.LogicalPin(Samha1g16aXproBindings.LED0_PORT_BASE, Samha1g16aXproBindings.LED0_PIN);

        /// <summary>The SW0 user button on PB03, ACTIVE LOW: pressing it drives the line to ground,
        /// so a pressed button reads <see cref="PinValue.Low"/>.</summary>
        public static readonly int ButtonPin =
            Samha1GpioDriver.LogicalPin(Samha1g16aXproBindings.BUTTON0_PORT_BASE, Samha1g16aXproBindings.BUTTON0_PIN);

        /// <summary>The mode the user button wants.</summary>
        /// <remarks>THIS KIT'S GUIDE DOES NOT SAY WHETHER AN EXTERNAL PULL-UP IS FITTED, where the
        /// Curiosity Nano guides state outright that none is. <see cref="PinMode.InputPullUp"/> is
        /// nevertheless the right answer under BOTH readings, which is why it can be offered without
        /// settling the question: if the board fits no pull-up the internal one is required, and if
        /// it fits one the internal resistor pulls the same way and changes nothing. A plain
        /// <see cref="PinMode.Input"/> is the only choice that depends on the unknown.</remarks>
        public static readonly PinMode ButtonMode = PinMode.InputPullUp;

        /// <summary>Binds this board's GPIO block to the driver table, so a program writes plain
        /// dotnet/iot -- <c>new GpioController()</c> -- and never names a Lamella type.</summary>
        /// <remarks>A TYPE INITIALIZER rather than the instance constructor, for the reason
        /// <see cref="Lamella.Hardware.Buses.BindGpio"/> documents: the table refuses a second bind
        /// of the same kind rather than replacing it, and this class is instantiable and routinely
        /// constructed as a temporary. The bound value is a FACTORY, so a program that never
        /// touches GPIO never constructs a driver.</remarks>
        static Samha1g16aXpro()
        {
            Buses.BindGpio(new GpioDriverFactory(MakeGpio));
        }

        private static GpioDriver MakeGpio() { return new Samha1GpioDriver(); }

        /// <summary>The family PORT driver this board bound.</summary>
        /// <remarks>THE DRIVER SPANS BOTH PORT GROUPS AND THIS PACKAGE BONDS PART OF EACH. A pin
        /// number the package does not carry is addressable and connected to nothing, which is why
        /// the constants above name only pads this board's own guide names.</remarks>
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
