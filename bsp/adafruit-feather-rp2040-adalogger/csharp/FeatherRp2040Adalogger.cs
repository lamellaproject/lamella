// Lamella.Boards.Adafruit.FeatherRp2040Adalogger -- the Adafruit Feather RP2040 Adalogger.
using System.Device.Gpio;
using Lamella.Generated;
using Lamella.Hardware;

namespace Lamella.Boards.Adafruit
{
    public sealed class FeatherRp2040Adalogger
    {
        /// <summary>The wire identity this board advertises (lamella_wire::board_model).</summary>
        public static readonly int BoardModel = FeatherRp2040AdaloggerBindings.BOARD_MODEL;

        /// <summary>The red user LED (Arduino D13) -- the board's blink target.</summary>
        public static readonly int LedPin = (int)FeatherRp2040AdaloggerBindings.LED_PIN;

        /// <summary>The on-board addressable RGB LED. ONE pin carries a timed serial protocol
        /// rather than a level, so a driver owns the waveform -- this is only the pin that reaches
        /// it.</summary>
        public static readonly int NeoPixelPin = (int)FeatherRp2040AdaloggerBindings.NEOPIXEL_PIN;

        /// <summary>The BOOT button, which the board wires ACTIVE LOW -- so a pressed button reads
        /// <see cref="PinValue.Low"/>, and the pin wants <see cref="PinMode.InputPullUp"/>.</summary>
        public static readonly int ButtonPin = (int)FeatherRp2040AdaloggerBindings.BUTTON_PIN;

        /// <summary>The microSD socket's card-detect line, ACTIVE LOW: a card present reads
        /// <see cref="PinValue.Low"/>.</summary>
        public static readonly int SdCardDetectPin =
            (int)FeatherRp2040AdaloggerBindings.SD_CARD_DETECT_PIN;

        /// <summary>Binds this board's GPIO block to the driver table, so a program writes plain
        /// dotnet/iot -- <c>new GpioController()</c> -- and never names a Lamella type. Touching
        /// <see cref="FeatherRp2040Adalogger"/> at all is what arms it, which is why a program
        /// constructs the board first.</summary>
        /// <remarks>A TYPE INITIALIZER rather than the instance constructor, for the reason
        /// <see cref="Lamella.Hardware.Buses.BindGpio"/> documents: the table refuses a second bind
        /// of the same kind rather than replacing it, and this class is instantiable and routinely
        /// constructed as a temporary. The language runs a type initializer once per program, so
        /// idempotence costs nothing and the table keeps its throw as a genuine-error detector.
        /// The bound value is a FACTORY, not a driver, so a program that never touches GPIO never
        /// constructs one.</remarks>
        static FeatherRp2040Adalogger()
        {
            Buses.BindGpio(new GpioDriverFactory(MakeGpio));
        }

        private static GpioDriver MakeGpio() { return new Rp2040GpioDriver(); }

        /// <summary>The family SIO/pad driver this board bound, over GP0..GP29.</summary>
        /// <remarks>THE SAME INSTANCE <see cref="GpioController"/> drives. One block has one
        /// driver, and handing out a second one over the same registers reads as working while the
        /// facade talks to the first -- see <see cref="Lamella.Hardware.Buses.ResolveSpi"/> for the
        /// full argument.</remarks>
        public GpioDriver CreateGpioDriver()
        {
            return Buses.ResolveGpio();
        }

        /// <summary>A GPIO controller over the RP2040 SIO/pad block.</summary>
        public GpioController CreateGpioController()
        {
            return new GpioController();
        }

        /// <summary>The UART0 FIFO depth, single-sourced from the block layout.</summary>
        public static readonly int UartFifoDepth = (int)Rp2040UartLayout.FIFO_DEPTH;

        /// <summary>UART0 on the board's TX/RX header pins, clocked from the crystal-exact
        /// clk_peri, ready for <c>Init(baud)</c>.</summary>
        public Rp2040Uart CreateUart()
        {
            return new Rp2040Uart(new Rp2040UartBinding(
                FeatherRp2040AdaloggerBindings.UART0_BASE,
                FeatherRp2040AdaloggerBindings.UART0_RESET_MASK,
                FeatherRp2040AdaloggerBindings.UART0_IO_TX_CTRL,
                FeatherRp2040AdaloggerBindings.UART0_IO_RX_CTRL,
                FeatherRp2040AdaloggerBindings.UART0_FUNCSEL,
                FeatherRp2040AdaloggerBindings.UART0_CLK_PERI_HZ));
        }
    }
}
