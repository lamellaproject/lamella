// Lamella.Boards.Microchip.Samd20Xpro -- the SAM D20 Xplained Pro (ATSAMD20-XPRO, ATSAMD20J18A).
using System.Device.Gpio;
using Lamella.Generated;
using Lamella.Hardware;

namespace Lamella.Boards.Microchip
{
    public sealed class Samd20Xpro
    {
        /// <summary>The wire identity this board advertises (lamella_wire::product_model).</summary>
        public static readonly int BoardModel = Samd20XproBindings.BOARD_MODEL;

        /// <summary>The yellow user LED LED0 on PA14, ACTIVE LOW: it lights when driven
        /// <see cref="PinValue.Low"/>.</summary>
        public static readonly int LedPin =
            Samd20GpioDriver.LogicalPin(Samd20XproBindings.LED0_PORT_BASE, Samd20XproBindings.LED0_PIN);

        /// <summary>The SW0 user button on PA15, ACTIVE LOW: pressing it drives the line to ground,
        /// so a pressed button reads <see cref="PinValue.Low"/>.</summary>
        public static readonly int ButtonPin =
            Samd20GpioDriver.LogicalPin(Samd20XproBindings.BUTTON0_PORT_BASE, Samd20XproBindings.BUTTON0_PIN);

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
        static Samd20Xpro()
        {
            Buses.BindGpio(new GpioDriverFactory(MakeGpio));
        }

        private static GpioDriver MakeGpio() { return new Samd20GpioDriver(); }

        /// <summary>The family PORT driver this board bound.</summary>
        /// <remarks>THE DRIVER SPANS BOTH PORT GROUPS AND THIS PACKAGE BONDS PART OF EACH. A pin
        /// number the package does not carry is addressable and connected to nothing, which is why
        /// the constants above name only pads this board's own guide names. Unlike the SAM R21
        /// beside it, no pad of this part is reserved by on-package silicon, so the driver refuses
        /// no pin by name -- the only pads to avoid are PA00 and PA01, which this KIT populates with
        /// its 32.768 kHz crystal. That is a board fact, and it is recorded in board.toml.</remarks>
        public GpioDriver CreateGpioDriver()
        {
            return Buses.ResolveGpio();
        }

        /// <summary>A GPIO controller over the SAM D20's PORT block.</summary>
        public GpioController CreateGpioController()
        {
            return new GpioController();
        }
    }
}
