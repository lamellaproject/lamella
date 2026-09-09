// Lamella.Boards.Microchip.Samg55Xpro -- the SAM G55 Xplained Pro (ATSAMG55-XPRO, ATSAMG55J19).
using System.Device.Gpio;
using Lamella.Generated;
using Lamella.Hardware;

namespace Lamella.Boards.Microchip
{
    public sealed class Samg55Xpro
    {
        /// <summary>The wire identity this board advertises (lamella_wire::product_model).</summary>
        public static readonly int BoardModel = Samg55XproBindings.BOARD_MODEL;

        /// <summary>The yellow user LED LED0 on PA6, ACTIVE LOW: it lights when driven
        /// <see cref="PinValue.Low"/>.</summary>
        /// <remarks>The kit guide spells this pad PA06 and the datasheet spells it PA6. Same pad;
        /// this tree follows the datasheet, as it does for every SAM3/SAM4-architecture part.</remarks>
        public static readonly int LedPin =
            Samg55GpioDriver.LogicalPin(Samg55XproBindings.LED0_PORT_BASE, Samg55XproBindings.LED0_PIN);

        /// <summary>The SW0 user button on PA2, ACTIVE LOW: pressing it drives the line to ground,
        /// so a pressed button reads <see cref="PinValue.Low"/>.</summary>
        public static readonly int ButtonPin =
            Samg55GpioDriver.LogicalPin(Samg55XproBindings.BUTTON0_PORT_BASE, Samg55XproBindings.BUTTON0_PIN);

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
        static Samg55Xpro()
        {
            Buses.BindGpio(new GpioDriverFactory(MakeGpio));
        }

        private static GpioDriver MakeGpio() { return new Samg55GpioDriver(); }

        /// <summary>The family PIO driver this board bound.</summary>
        /// <remarks>THE DRIVER SPANS 48 LINES AND THIS PACKAGE CARRIES ALL OF THEM -- PA0..PA31 on
        /// PIOA and PB0..PB15 on PIOB, which Table 1-1's count of 48 confirms -- so no logical pin
        /// number this driver accepts is unbonded. Four of PIOB's
        /// lines are the JTAG and SWD pads and one is the ERASE pin, so "carried by the package" and
        /// "free for a program" are still different questions, and the constants above name only
        /// pads this board's own guide names.</remarks>
        public GpioDriver CreateGpioDriver()
        {
            return Buses.ResolveGpio();
        }

        /// <summary>A GPIO controller over the SAM G55's PIO controllers.</summary>
        public GpioController CreateGpioController()
        {
            return new GpioController();
        }
    }
}
