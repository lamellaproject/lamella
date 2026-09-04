// Lamella.Boards.RaspberryPi.PicoW -- the Raspberry Pi Pico W (RP2040 + an Infineon CYW43439).
using System.Device.Gpio;
using Lamella.Generated;
using Lamella.Hardware;

namespace Lamella.Boards.RaspberryPi
{
    public sealed class PicoW
    {
        /// <summary>The wire identity this board advertises (lamella_wire::board_model).</summary>
        public static readonly int BoardModel = PicoWBindings.BOARD_MODEL;

        /// <summary>The pins the CYW43439 owns, as a mask over the user bank -- WL_REG_ON, the
        /// shared data/IRQ line, the chip select and the clock. Composed from the generated
        /// per-line masks rather than written out, so a line moved in board.toml moves here.</summary>
        public static readonly uint RadioPins =
            PicoWBindings.CYW43439_WL_REG_ON_MASK
            | PicoWBindings.CYW43439_DATA_MASK
            | PicoWBindings.CYW43439_CS_MASK
            | PicoWBindings.CYW43439_CLK_MASK;

        /// <summary>Binds this board's GPIO block to the driver table, so a program writes plain
        /// dotnet/iot -- <c>new GpioController()</c> -- and never names a Lamella type. Touching
        /// <see cref="PicoW"/> at all is what arms it, which is why a program constructs the board
        /// first.</summary>
        /// <remarks>A TYPE INITIALIZER rather than the instance constructor, for the reason
        /// <see cref="Lamella.Hardware.Buses.BindGpio"/> documents: the table refuses a second bind
        /// of the same kind rather than replacing it, and this class is instantiable and routinely
        /// constructed as a temporary. The language runs a type initializer once per program, so
        /// idempotence costs nothing and the table keeps its throw as a genuine-error detector.
        /// The bound value is a FACTORY, not a driver, so a program that never touches GPIO never
        /// constructs one.</remarks>
        static PicoW()
        {
            Buses.BindGpio(new GpioDriverFactory(MakeGpio));
        }

        private static GpioDriver MakeGpio() { return new Rp2040GpioDriver(RadioPins); }

        /// <summary>The family SIO/pad driver this board bound, over GP0..GP29 less the four lines
        /// in <see cref="RadioPins"/>.</summary>
        /// <remarks>THE SAME INSTANCE <see cref="GpioController"/> drives. One block has one
        /// driver, and handing out a second one over the same registers reads as working while the
        /// facade talks to the first -- see <see cref="Lamella.Hardware.Buses.ResolveSpi"/> for the
        /// full argument.</remarks>
        public GpioDriver CreateGpioDriver()
        {
            return Buses.ResolveGpio();
        }

        /// <summary>A GPIO controller over the RP2040 SIO/pad block. The radio's four lines refuse
        /// <c>SetPinMode</c>.</summary>
        public GpioController CreateGpioController()
        {
            return new GpioController();
        }

        /// <summary>The UART0 FIFO depth, single-sourced from the block layout.</summary>
        public static readonly int UartFifoDepth = (int)Rp2040UartLayout.FIFO_DEPTH;

        /// <summary>UART0 on GP0 (TX, header pin 1) / GP1 (RX, pin 2), clocked from the
        /// crystal-exact clk_peri, ready for <c>Init(baud)</c>.</summary>
        public Rp2040Uart CreateUart()
        {
            return new Rp2040Uart(new Rp2040UartBinding(
                PicoWBindings.UART0_BASE,
                PicoWBindings.UART0_RESET_MASK,
                PicoWBindings.UART0_IO_TX_CTRL,
                PicoWBindings.UART0_IO_RX_CTRL,
                PicoWBindings.UART0_FUNCSEL,
                PicoWBindings.UART0_CLK_PERI_HZ));
        }
    }
}
