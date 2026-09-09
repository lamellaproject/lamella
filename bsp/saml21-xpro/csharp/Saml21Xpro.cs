// Lamella.Boards.Microchip.Saml21Xpro -- the SAM L21 Xplained Pro (ATSAML21-XPRO-B, ATSAML21J18B).
using System.Device.Gpio;
using Lamella.Generated;
using Lamella.Hardware;

namespace Lamella.Boards.Microchip
{
    public sealed class Saml21Xpro
    {
        /// <summary>The wire identity this board advertises (lamella_wire::product_model).</summary>
        public static readonly int BoardModel = Saml21XproBindings.BOARD_MODEL;

        /// <summary>The yellow user LED LED0 on PB10, ACTIVE LOW: it lights when driven
        /// <see cref="PinValue.Low"/>.</summary>
        /// <remarks>SHARED WITH EXT3, per the kit guide's own Shared functionality column, so a
        /// program driving the LED is also driving whatever an extension board presents on that
        /// line.</remarks>
        public static readonly int LedPin =
            Saml21GpioDriver.LogicalPin(Saml21XproBindings.LED0_PORT_BASE, Saml21XproBindings.LED0_PIN);

        /// <summary>The SW0 user button on PA02, ACTIVE LOW: pressing it drives the line to ground,
        /// so a pressed button reads <see cref="PinValue.Low"/>.</summary>
        /// <remarks>SHARED WITH EXT1, per the guide's own column. PA02 is also this part's DAC
        /// output and an OPAMP input; nothing binds either, and the board file records that the pad
        /// carries unusually many claims so a later analog binding collides with the button rather
        /// than with nothing.</remarks>
        public static readonly int ButtonPin =
            Saml21GpioDriver.LogicalPin(Saml21XproBindings.BUTTON0_PORT_BASE, Saml21XproBindings.BUTTON0_PIN);

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
        static Saml21Xpro()
        {
            Buses.BindGpio(new GpioDriverFactory(MakeGpio));
        }

        private static GpioDriver MakeGpio() { return new Saml21GpioDriver(); }

        /// <summary>The family PORT driver this board bound.</summary>
        /// <remarks>THE DRIVER SPANS BOTH PORT GROUPS AND THIS PACKAGE BONDS PADS IN EACH. A pin
        /// number the package does not carry is addressable and connected to nothing, which is why
        /// the constants above name only pads this board's own guide names -- and on this family
        /// PA28 is not one of them at all, existing in no package of the part.</remarks>
        public GpioDriver CreateGpioDriver()
        {
            return Buses.ResolveGpio();
        }

        /// <summary>A GPIO controller over the SAM L21's PORT block.</summary>
        public GpioController CreateGpioController()
        {
            return new GpioController();
        }
    }
}
