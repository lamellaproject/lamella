// Lamella.Boards.Microchip.Samd21Cnano -- the SAM D21 Curiosity Nano (SAMD21-CNANO, ATSAMD21G17D).
using System.Device.Gpio;
using Lamella.Generated;
using Lamella.Hardware;

namespace Lamella.Boards.Microchip
{
    public sealed class Samd21Cnano
    {
        /// <summary>The wire identity this board advertises (lamella_wire::product_model).</summary>
        public static readonly int BoardModel = Samd21CnanoBindings.BOARD_MODEL;

        /// <summary>The yellow user LED LED0 on PB10, ACTIVE LOW: it lights when driven
        /// <see cref="PinValue.Low"/>.</summary>
        /// <remarks>This pad has no shared functionality -- the guide's own column says so -- so
        /// nothing else on the board contends for it. The user switch is not in that position.</remarks>
        public static readonly int LedPin =
            Samd21GpioDriver.LogicalPin(Samd21CnanoBindings.LED0_PORT_BASE, Samd21CnanoBindings.LED0_PIN);

        /// <summary>The SW0 user switch on PB11, ACTIVE LOW: pressing it drives the line to ground,
        /// so a pressed switch reads <see cref="PinValue.Low"/>.</summary>
        /// <remarks>OPEN IT WITH <see cref="ButtonMode"/>. There is no pull-up resistor on this
        /// board, so a plain input floats. THIS PAD IS SHARED: it is the debugger's DBG2 line as
        /// well as the switch, so a host-side tool driving that line and a program reading the
        /// button are reaching for the same pad.</remarks>
        public static readonly int ButtonPin =
            Samd21GpioDriver.LogicalPin(Samd21CnanoBindings.BUTTON0_PORT_BASE, Samd21CnanoBindings.BUTTON0_PIN);

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
        static Samd21Cnano()
        {
            Buses.BindGpio(new GpioDriverFactory(MakeGpio));
        }

        private static GpioDriver MakeGpio() { return new Samd21GpioDriver(); }

        /// <summary>The family PORT driver this board bound.</summary>
        /// <remarks>THE DRIVER SPANS BOTH PORT GROUPS AND THIS PACKAGE BONDS PART OF EACH. A pin
        /// number the package does not carry is addressable and connected to nothing, which is why
        /// the constants above name only pads this board's own guide names against this part
        /// number.</remarks>
        public GpioDriver CreateGpioDriver()
        {
            return Buses.ResolveGpio();
        }

        /// <summary>A GPIO controller over the SAM D21's PORT block.</summary>
        public GpioController CreateGpioController()
        {
            return new GpioController();
        }
    }
}
