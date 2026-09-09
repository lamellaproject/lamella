// Lamella.Boards.Microchip.Samr30Xpro -- the SAM R30 Xplained Pro (ATSAMR30-XPRO, ATSAMR30G18A).
using System.Device.Gpio;
using Lamella.Generated;
using Lamella.Hardware;

namespace Lamella.Boards.Microchip
{
    public sealed class Samr30Xpro
    {
        /// <summary>The wire identity this board advertises (lamella_wire::product_model).</summary>
        public static readonly int BoardModel = Samr30XproBindings.BOARD_MODEL;

        /// <summary>The soft orange user LED LED0 on PA19, ACTIVE LOW: it lights when driven
        /// <see cref="PinValue.Low"/>.</summary>
        public static readonly int Led0Pin =
            Samr30GpioDriver.LogicalPin(Samr30XproBindings.LED0_PORT_BASE, Samr30XproBindings.LED0_PIN);

        /// <summary>The green user LED LED1 on PA18, ACTIVE LOW: it lights when driven
        /// <see cref="PinValue.Low"/>.</summary>
        public static readonly int Led1Pin =
            Samr30GpioDriver.LogicalPin(Samr30XproBindings.LED1_PORT_BASE, Samr30XproBindings.LED1_PIN);

        /// <summary>The SW0 user button on PA28, ACTIVE LOW: pressing it drives the line to ground,
        /// so a pressed button reads <see cref="PinValue.Low"/>.</summary>
        public static readonly int ButtonPin =
            Samr30GpioDriver.LogicalPin(Samr30XproBindings.BUTTON0_PORT_BASE, Samr30XproBindings.BUTTON0_PIN);

        /// <summary>The mode the user button wants.</summary>
        /// <remarks>REQUIRED HERE RATHER THAN MERELY SAFE, WHICH IS NOT TRUE OF THE SIBLING KITS.
        /// The SAM R21 and SAM L21 Xplained Pro guides state the button's polarity and stop, leaving
        /// an external pull-up an open question that <see cref="PinMode.InputPullUp"/> happens to
        /// answer correctly either way. THIS guide closes it: "There is no pull-up resistor connected
        /// to the generic user button. Remember to enable the internal pull-up in the SAM R30 to use
        /// the button." So a plain <see cref="PinMode.Input"/> here reads a floating pin, and this
        /// is the one board of the three where that is known rather than suspected.</remarks>
        public static readonly PinMode ButtonMode = PinMode.InputPullUp;

        /// <summary>Binds this board's GPIO block to the driver table, so a program writes plain
        /// dotnet/iot -- <c>new GpioController()</c> -- and never names a Lamella type.</summary>
        /// <remarks>A TYPE INITIALIZER rather than the instance constructor, for the reason
        /// <see cref="Lamella.Hardware.Buses.BindGpio"/> documents: the table refuses a second bind
        /// of the same kind rather than replacing it, and this class is instantiable and routinely
        /// constructed as a temporary. The bound value is a FACTORY, so a program that never
        /// touches GPIO never constructs a driver.</remarks>
        static Samr30Xpro()
        {
            Buses.BindGpio(new GpioDriverFactory(MakeGpio));
        }

        private static GpioDriver MakeGpio() { return new Samr30GpioDriver(); }

        /// <summary>The family PORT driver this board bound.</summary>
        /// <remarks>THE DRIVER SPANS ALL THREE PORT GROUPS AND THIS PACKAGE BONDS PART OF TWO. A pin
        /// number the package does not carry is addressable and connected to nothing, which is why
        /// the constants above name only pads this board's own guide names. Four of the part's pads
        /// -- PA10, PA11, PB16 and PB17 -- are wired to the transceiver's control inputs and cannot
        /// be driven at all; the driver refuses an output on them by name rather than storing to a
        /// direction register that will not take it. Two more, PA09 and PA12, are this BOARD's RF
        /// switch controls rather than the part's, so they are free on a bare chip and spoken for
        /// here.</remarks>
        public GpioDriver CreateGpioDriver()
        {
            return Buses.ResolveGpio();
        }

        /// <summary>A GPIO controller over the SAM R30's PORT block.</summary>
        public GpioController CreateGpioController()
        {
            return new GpioController();
        }
    }
}
