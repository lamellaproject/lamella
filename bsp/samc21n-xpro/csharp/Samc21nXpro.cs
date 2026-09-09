// Lamella.Boards.Microchip.Samc21nXpro -- the SAMC21N Xplained Pro (ATSAMC21N-XPRO, ATSAMC21N18A).
using System.Device.Gpio;
using Lamella.Generated;
using Lamella.Hardware;

namespace Lamella.Boards.Microchip
{
    public sealed class Samc21nXpro
    {
        /// <summary>The wire identity this board advertises (lamella_wire::product_model).</summary>
        public static readonly int BoardModel = Samc21nXproBindings.BOARD_MODEL;

        /// <summary>The yellow user LED on PC05, ACTIVE LOW: it lights when driven
        /// <see cref="PinValue.Low"/>.</summary>
        /// <remarks>ON PORT GROUP C, which only this package bonds. Driven as a plain GPIO here
        /// despite the guide naming the pad's timer function.</remarks>
        public static readonly int LedPin =
            Samc21GpioDriver.LogicalPin(Samc21nXproBindings.LED0_PORT_BASE, Samc21nXproBindings.LED0_PIN);

        /// <summary>The user button on PB19, ACTIVE LOW: pressing it drives the line to ground, so a
        /// pressed button reads <see cref="PinValue.Low"/>.</summary>
        /// <remarks>The guide calls it "GPIO for User Button" and silkscreens no SW0, unlike this
        /// family's other kit.</remarks>
        public static readonly int ButtonPin =
            Samc21GpioDriver.LogicalPin(Samc21nXproBindings.BUTTON0_PORT_BASE, Samc21nXproBindings.BUTTON0_PIN);

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
        static Samc21nXpro()
        {
            Buses.BindGpio(new GpioDriverFactory(MakeGpio));
        }

        private static GpioDriver MakeGpio() { return new Samc21GpioDriver(); }

        /// <summary>The family PORT driver this board bound.</summary>
        /// <remarks>THE DRIVER SPANS THREE PORT GROUPS AND THIS PACKAGE BONDS PADS IN ALL THREE.
        /// This part also carries two SERCOM instances the 64-pin sibling does not -- SERCOM6 and
        /// SERCOM7, behind a fourth APB bridge -- and nothing on this board binds either.</remarks>
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
