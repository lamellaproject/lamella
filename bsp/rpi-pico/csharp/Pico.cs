// Lamella.Boards.RaspberryPi.Pico -- the original Raspberry Pi Pico / Pico H (RP2040) board-support
using System.Device.Gpio;
using Lamella.Generated;
using Lamella.Hardware;

namespace Lamella.Boards.RaspberryPi
{
    public sealed class Pico
    {
        /// <summary>The wire identity this board advertises (lamella_wire::product_model).</summary>
        public static readonly int BoardModel = PicoBindings.BOARD_MODEL;

        /// <summary>The green user LED on GP25 -- the board's blink target, and the only indicator
        /// it has. Lifted from the generated pin rather than written out, so a pin moved in
        /// board.toml moves here.</summary>
        public static readonly int LedPin = (int)PicoBindings.LED_PIN;

        /// <summary>Binds this board's GPIO block to the driver table, so a program writes plain
        /// dotnet/iot -- <c>new GpioController()</c> -- and never names a Lamella type. Touching
        /// <see cref="Pico"/> at all is what arms it, which is why a program constructs the board
        /// first.</summary>
        /// <remarks>A TYPE INITIALIZER rather than the instance constructor, for the reason
        /// <see cref="Lamella.Hardware.Buses.BindGpio"/> documents: the table refuses a second bind
        /// of the same kind rather than replacing it, and this class is instantiable and routinely
        /// constructed as a temporary. The language runs a type initializer once per program, so
        /// idempotence costs nothing and the table keeps its throw as a genuine-error detector.
        /// The bound value is a FACTORY, not a driver, so a program that never touches GPIO never
        /// constructs one.</remarks>
        static Pico()
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

        /// <summary>UART0 on GP0 (TX, header pin 1) / GP1 (RX, pin 2), clocked from the
        /// crystal-exact clk_peri under the xosc-12mhz plan, ready for <c>Init(baud)</c>.</summary>
        public Rp2040Uart CreateUart()
        {
            return new Rp2040Uart(new Rp2040UartBinding(
                PicoBindings.UART0_BASE,
                PicoBindings.UART0_RESET_MASK,
                PicoBindings.UART0_IO_TX_CTRL,
                PicoBindings.UART0_IO_RX_CTRL,
                PicoBindings.UART0_FUNCSEL,
                PicoBindings.UART0_CLK_PERI_HZ));
        }
    }
}
