// Lamella.Boards.Microchip.Same51Cnano -- the SAM E51 Curiosity Nano (EV76S68A, ATSAME51J20A).
using System.Device.Gpio;
using Lamella.Generated;
using Lamella.Hardware;

namespace Lamella.Boards.Microchip
{
    public sealed class Same51Cnano
    {
        /// <summary>The wire identity this board advertises (lamella_wire::product_model).</summary>
        public static readonly int BoardModel = Same51CnanoBindings.BOARD_MODEL;

        /// <summary>The yellow user LED LD0 on PA14, ACTIVE LOW: it lights when driven
        /// <see cref="PinValue.Low"/>.</summary>
        /// <remarks>Unlike the Xplained Pro board's, this pad has no shared functionality -- the
        /// guide's own column says so -- so nothing else on the board contends for it.</remarks>
        public static readonly int LedPin =
            Same54GpioDriver.LogicalPin(Same51CnanoBindings.LED0_PORT_BASE, Same51CnanoBindings.LED0_PIN);

        /// <summary>The SW0 user switch on PA15, ACTIVE LOW: pressing it drives the line to ground,
        /// so a pressed switch reads <see cref="PinValue.Low"/>.</summary>
        /// <remarks>OPEN IT WITH <see cref="ButtonMode"/>. There is no pull-up resistor on this
        /// board, so a plain input floats.</remarks>
        public static readonly int ButtonPin =
            Same54GpioDriver.LogicalPin(Same51CnanoBindings.BUTTON0_PORT_BASE, Same51CnanoBindings.BUTTON0_PIN);

        /// <summary>The mode the user switch needs: the board provides no pull-up, so the internal
        /// one carries the line while the switch is not pressed.</summary>
        public static readonly PinMode ButtonMode = PinMode.InputPullUp;

        /// <summary>Binds this board's GPIO block to the driver table, so a program writes plain
        /// dotnet/iot -- <c>new GpioController()</c> -- and never names a Lamella type.</summary>
        /// <remarks>A TYPE INITIALIZER rather than the instance constructor, for the reason
        /// <see cref="Lamella.Hardware.Buses.BindGpio"/> documents: the table refuses a second bind
        /// of the same kind rather than replacing it, and this class is instantiable and routinely
        /// constructed as a temporary. The bound value is a FACTORY, so a program that never
        /// touches GPIO never constructs a driver.</remarks>
        static Same51Cnano()
        {
            Buses.BindGpio(new GpioDriverFactory(MakeGpio));
        }

        private static GpioDriver MakeGpio() { return new Same54GpioDriver(); }

        /// <summary>The family GPIO driver this board bound.</summary>
        /// <remarks>THE DRIVER SPANS THE FAMILY'S FOUR PORT GROUPS AND THIS PACKAGE BONDS TWO.
        /// Ports C and D exist in the address map and reach no pin on a 64-pin part, so a pin
        /// number in them is addressable and connected to nothing. The constants above name only
        /// pads this package carries, which is the protection a program actually gets.</remarks>
        public GpioDriver CreateGpioDriver()
        {
            return Buses.ResolveGpio();
        }

        /// <summary>A GPIO controller over the SAM E51's PORT block.</summary>
        public GpioController CreateGpioController()
        {
            return new GpioController();
        }
    }
}
