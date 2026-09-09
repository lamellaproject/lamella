// Lamella.Boards.Microchip.Saml22Xpro -- the SAM L22 Xplained Pro (ATSAML22-XPRO-B, ATSAML22N18A).
using System.Device.Gpio;
using Lamella.Generated;
using Lamella.Hardware;

namespace Lamella.Boards.Microchip
{
    public sealed class Saml22Xpro
    {
        /// <summary>The wire identity this board advertises (lamella_wire::product_model).</summary>
        public static readonly int BoardModel = Saml22XproBindings.BOARD_MODEL;

        /// <summary>The yellow user LED LED0 on PC27, ACTIVE LOW: it lights when driven
        /// <see cref="PinValue.Low"/>.</summary>
        /// <remarks>SHARED WITH THE SEGMENT LCD, per the guide's own column.</remarks>
        public static readonly int LedPin =
            Saml22GpioDriver.LogicalPin(Saml22XproBindings.LED0_PORT_BASE, Saml22XproBindings.LED0_PIN);

        /// <summary>The SW0 user button on PC01, ACTIVE LOW: pressing it drives the line to ground, so a
        /// pressed button reads <see cref="PinValue.Low"/>.</summary>
        /// <remarks>SHARED THREE WAYS -- EXT2, the shield header and an EDBG GPIO -- per the
        /// guide's own column, which is more claims than any other button on this roster.</remarks>
        public static readonly int ButtonPin =
            Saml22GpioDriver.LogicalPin(Saml22XproBindings.BUTTON0_PORT_BASE, Saml22XproBindings.BUTTON0_PIN);

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
        static Saml22Xpro()
        {
            Buses.BindGpio(new GpioDriverFactory(MakeGpio));
        }

        private static GpioDriver MakeGpio() { return new Saml22GpioDriver(); }

        /// <summary>The family PORT driver this board bound.</summary>
        /// <remarks>THE DRIVER SPANS THREE PORT GROUPS AND THIS PACKAGE BONDS PADS IN ALL THREE.
        /// This part also carries six SERCOM instances where the smaller packages of the same family
        /// carry four, so a binding written here does not transfer down to an L22G or L22J.</remarks>
        public GpioDriver CreateGpioDriver()
        {
            return Buses.ResolveGpio();
        }

        /// <summary>A GPIO controller over the SAM L22's PORT block.</summary>
        public GpioController CreateGpioController()
        {
            return new GpioController();
        }
    }
}
