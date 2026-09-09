// Lamella.Boards.Microchip.Saml11Xpro -- the SAM L11 Xplained Pro (ATSAML11-XPRO, ATSAML11E16A).
using System.Device.Gpio;
using Lamella.Generated;
using Lamella.Hardware;

namespace Lamella.Boards.Microchip
{
    public sealed class Saml11Xpro
    {
        /// <summary>The wire identity this board advertises (lamella_wire::product_model).</summary>
        public static readonly int BoardModel = Saml11XproBindings.BOARD_MODEL;

        /// <summary>The yellow user LED LED0 on PA07, ACTIVE LOW: it lights when driven
        /// <see cref="PinValue.Low"/>.</summary>
        /// <remarks>THIS LINE IS ALSO THE DEBUGGER'S SPI SLAVE SELECT. The kit guide's Shared
        /// Functionality column reads "DGI SPI", so a program blinking this LED is also driving the
        /// data gateway's chip select.</remarks>
        public static readonly int LedPin =
            Saml1xGpioDriver.LogicalPin(Saml11XproBindings.LED0_PORT_BASE, Saml11XproBindings.LED0_PIN);

        /// <summary>The SW0 user button on PA27, ACTIVE LOW: pressing it drives the line to ground,
        /// so a pressed button reads <see cref="PinValue.Low"/>.</summary>
        public static readonly int ButtonPin =
            Saml1xGpioDriver.LogicalPin(Saml11XproBindings.BUTTON0_PORT_BASE, Saml11XproBindings.BUTTON0_PIN);

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
        static Saml11Xpro()
        {
            Buses.BindGpio(new GpioDriverFactory(MakeGpio));
        }

        private static GpioDriver MakeGpio() { return new Saml1xGpioDriver(); }

        /// <summary>The family PORT driver this board bound.</summary>
        /// <remarks>THE DRIVER SPANS ALL 32 PINS AND THIS PACKAGE BONDS 25 OF THEM. A pin number the
        /// package does not carry is addressable and connected to nothing, which is why the
        /// constants above name only pads this board's own guide names. PA30 and PA31 are the
        /// board's ONLY debug path and they arrive muxed to it -- PMUX15 resets to function G -- so
        /// a program opening either as general-purpose I/O ends its own debug session.</remarks>
        public GpioDriver CreateGpioDriver()
        {
            return Buses.ResolveGpio();
        }

        /// <summary>A GPIO controller over the SAM L11's PORT block.</summary>
        public GpioController CreateGpioController()
        {
            return new GpioController();
        }
    }
}
