// Lamella.Boards.Microchip.Samc21Xpro -- the SAM C21 Xplained Pro (ATSAMC21-XPRO, ATSAMC21J18A).
using System.Device.Gpio;
using Lamella.Generated;
using Lamella.Hardware;

namespace Lamella.Boards.Microchip
{
    public sealed class Samc21Xpro
    {
        /// <summary>The wire identity this board advertises (lamella_wire::product_model).</summary>
        public static readonly int BoardModel = Samc21XproBindings.BOARD_MODEL;

        /// <summary>The yellow user LED LED0 on PA15, ACTIVE LOW: it lights when driven
        /// <see cref="PinValue.Low"/>.</summary>
        /// <remarks>THIS PAD IS SHARED WITH EXT3 AND WITH A CRYSTAL FOOTPRINT, which the kit guide
        /// states in its own Shared functionality column. The footprint is unpopulated on a stock
        /// board -- the 32.768 kHz crystal that IS mounted is elsewhere -- so nothing contends for
        /// the pad as shipped; a board that populated it would be choosing between the two.</remarks>
        public static readonly int LedPin =
            Samc21GpioDriver.LogicalPin(Samc21XproBindings.LED0_PORT_BASE, Samc21XproBindings.LED0_PIN);

        /// <summary>The SW0 user button on PA28, ACTIVE LOW: pressing it drives the line to ground,
        /// so a pressed button reads <see cref="PinValue.Low"/>.</summary>
        /// <remarks>SHARED WITH EXT3 AND AN EDBG GPIO, per the guide's own column.</remarks>
        public static readonly int ButtonPin =
            Samc21GpioDriver.LogicalPin(Samc21XproBindings.BUTTON0_PORT_BASE, Samc21XproBindings.BUTTON0_PIN);

        /// <summary>The mode the user button wants.</summary>
        /// <remarks>THIS KIT'S GUIDE DOES NOT SAY WHETHER AN EXTERNAL PULL-UP IS FITTED, where the
        /// Curiosity Nano guides state outright that none is. <see cref="PinMode.InputPullUp"/> is
        /// nevertheless the right answer under BOTH readings: if the board fits no pull-up the
        /// internal one is required, and if it fits one the internal resistor pulls the same way and
        /// changes nothing. A plain <see cref="PinMode.Input"/> is the only choice that depends on
        /// the unknown.</remarks>
        public static readonly PinMode ButtonMode = PinMode.InputPullUp;

        /// <summary>Binds this board's GPIO block to the driver table, so a program writes plain
        /// dotnet/iot -- <c>new GpioController()</c> -- and never names a Lamella type.</summary>
        /// <remarks>A TYPE INITIALIZER rather than the instance constructor, for the reason
        /// <see cref="Lamella.Hardware.Buses.BindGpio"/> documents: the table refuses a second bind
        /// of the same kind rather than replacing it, and this class is instantiable and routinely
        /// constructed as a temporary. The bound value is a FACTORY, so a program that never
        /// touches GPIO never constructs a driver.</remarks>
        static Samc21Xpro()
        {
            Buses.BindGpio(new GpioDriverFactory(MakeGpio));
        }

        private static GpioDriver MakeGpio() { return new Samc21GpioDriver(); }

        /// <summary>The family PORT driver this board bound.</summary>
        /// <remarks>THE DRIVER SPANS THREE PORT GROUPS AND THIS PACKAGE BONDS PADS IN TWO. Group C
        /// exists on the die and reaches no pin on a 64-pin part, which is why the constants above
        /// name only pads this board's own guide names. The sibling samc21n-xpro is the same family
        /// on the 100-pin package and puts its LED in that third group.</remarks>
        public GpioDriver CreateGpioDriver()
        {
            return Buses.ResolveGpio();
        }

        /// <summary>A GPIO controller over the SAM C21's PORT block.</summary>
        public GpioController CreateGpioController()
        {
            return new GpioController();
        }
    }
}
