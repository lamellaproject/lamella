// Lamella.Boards.Adafruit.FeatherM0Express -- the Adafruit Feather M0 Express (ATSAMD21G18A, Cortex-M0+).
using System.Device.Gpio;
using Lamella.Generated;
using Lamella.Hardware;

namespace Lamella.Boards.Adafruit
{
    public sealed class FeatherM0Express
    {
        /// <summary>The wire identity this board advertises (lamella_wire::product_model).</summary>
        public static readonly int BoardModel = FeatherM0ExpressBindings.BOARD_MODEL;

        /// <summary>Binds this board's GPIO block to the driver table, so a program writes plain
        /// dotnet/iot -- <c>new GpioController()</c> -- and never names a Lamella type. Touching
        /// <see cref="FeatherM0Express"/> at all is what arms it, which is why a program constructs
        /// the board first.</summary>
        /// <remarks>A TYPE INITIALIZER rather than the instance constructor, for the reason
        /// <see cref="Lamella.Hardware.Buses.BindGpio"/> documents: the table refuses a second bind
        /// of the same kind rather than replacing it, and this class is instantiable and routinely
        /// constructed as a temporary. The language runs a type initializer once per program, so
        /// idempotence costs nothing and the table keeps its throw as a genuine-error detector.
        /// The bound value is a FACTORY, not a driver, so a program that never touches GPIO never
        /// constructs one.</remarks>
        static FeatherM0Express()
        {
            Buses.BindGpio(new GpioDriverFactory(MakeGpio));
        }

        private static GpioDriver MakeGpio() { return new Samd21GpioDriver(); }

        /// <summary>The red LED beside the USB jack (Arduino D13) -- the board's blink target, and
        /// the only indicator visible without driving a protocol.</summary>
        public static readonly int LedPin =
            LogicalPin(FeatherM0ExpressBindings.LED_PORT_BASE, FeatherM0ExpressBindings.LED_PIN);

        /// <summary>The on-board addressable RGB LED (Arduino D8). ONE pin carries a timed serial
        /// protocol rather than a level, so a driver owns the waveform -- this is only the pin that
        /// reaches it.</summary>
        public static readonly int NeoPixelPin =
            LogicalPin(FeatherM0ExpressBindings.NEOPIXEL_PORT_BASE, FeatherM0ExpressBindings.NEOPIXEL_PIN);

        /// <summary>The family's PORT driver, over every pin on the part.</summary>
        /// <remarks>THE SAME INSTANCE <see cref="GpioController"/> drives, resolved through the
        /// board's table rather than constructed here. One block has one driver, and handing out a
        /// second one over the same registers reads as working while the facade talks to the
        /// first -- see <see cref="Lamella.Hardware.Buses.ResolveSpi"/> for the full argument.</remarks>
        public GpioDriver CreateGpioDriver()
        {
            return Buses.ResolveGpio();
        }

        /// <summary>A controller over that driver, for callers who want the pin-object surface.</summary>
        public GpioController CreateGpioController()
        {
            return new GpioController();
        }

        /// <summary>The driver's logical numbering: group * 32 + pin, with the group taken from the
        /// binding's port base. Kept here rather than in the driver because it converts a BOARD fact
        /// (which port a device sits on) into the family's pin space.</summary>
        static int LogicalPin(uint portBase, uint pin)
        {
            int group = portBase == Samd21Instances.PORTB_BASE ? 1 : 0;
            return group * 32 + (int)pin;
        }
    }
}
