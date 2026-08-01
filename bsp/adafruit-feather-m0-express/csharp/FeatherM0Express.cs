// Lamella.Boards.Adafruit.FeatherM0Express -- the Adafruit Feather M0 Express (ATSAMD21G18A, Cortex-M0+).
using System.Device.Gpio;
using Lamella.Generated;

namespace Lamella.Boards.Adafruit
{
    public sealed class FeatherM0Express
    {
        /// <summary>The wire identity this board advertises (lamella_wire::board_model).</summary>
        public static readonly int BoardModel = FeatherM0ExpressBindings.BOARD_MODEL;

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
        public GpioDriver CreateGpioDriver()
        {
            return new Samd21GpioDriver();
        }

        /// <summary>A controller over that driver, for callers who want the pin-object surface.</summary>
        public GpioController CreateGpioController()
        {
            return new GpioController(PinNumberingScheme.Logical, new Samd21GpioDriver());
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
