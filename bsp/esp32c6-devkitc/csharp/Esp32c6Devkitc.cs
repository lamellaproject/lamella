// Lamella.Boards.Espressif.Esp32c6Devkitc -- the Espressif ESP32-C6-DevKitC-1 (RISC-V RV32IMAC) board
using System.Device.Gpio;
using Lamella.Generated;
using Lamella.Hardware;

namespace Lamella.Boards.Espressif
{
    public sealed class Esp32c6Devkitc
    {
        /// <summary>The wire identity this board advertises (lamella_wire::product_model).</summary>
        public static readonly int BoardModel = Esp32c6DevkitcBindings.BOARD_MODEL;

        /// <summary>Binds this board's GPIO block to the driver table, so a program writes plain
        /// dotnet/iot -- <c>new GpioController()</c> -- and never names a Lamella type. Touching
        /// <see cref="Esp32c6Devkitc"/> at all is what arms it, which is why a program constructs
        /// the board first.</summary>
        /// <remarks>A TYPE INITIALIZER rather than the instance constructor, for the reason
        /// <see cref="Lamella.Hardware.Buses.BindGpio"/> documents: the table refuses a second bind
        /// of the same kind rather than replacing it, and this class is instantiable and routinely
        /// constructed as a temporary. The language runs a type initializer once per program, so
        /// idempotence costs nothing and the table keeps its throw as a genuine-error detector.
        /// The bound value is a FACTORY, not a driver, so a program that never touches GPIO never
        /// constructs one.</remarks>
        static Esp32c6Devkitc()
        {
            Buses.BindGpio(new GpioDriverFactory(MakeGpio));
        }

        private static GpioDriver MakeGpio() { return new Esp32C6GpioDriver(); }

        /// <summary>The UART TX/RX FIFO depth, single-sourced from the block layout.</summary>
        public static readonly int UartFifoDepth = (int)Esp32c6UartLayout.FIFO_DEPTH;

        /// <summary>UART0 on its native IO_MUX pins (TX GPIO16 / RX GPIO17 -- the ROM console,
        /// routed to the on-board USB-UART bridge), ready for <c>Init(baud)</c>.</summary>
        public Esp32C6Uart CreateUart()
        {
            return new Esp32C6Uart(new Esp32C6UartBinding(
                Esp32c6DevkitcBindings.UART0_BASE,
                Esp32c6DevkitcBindings.UART0_PCR_CONF,
                Esp32c6DevkitcBindings.UART0_PCR_SCLK_CONF,
                Esp32c6DevkitcBindings.UART0_IO_MUX_TX,
                Esp32c6DevkitcBindings.UART0_IO_MUX_RX,
                Esp32c6DevkitcBindings.UART0_MCU_SEL,
                Esp32c6DevkitcBindings.UART0_SCLK_HZ));
        }

        /// <summary>The GPIO block (RGB LED on GPIO8 via RMT on the DevKit; general IO).</summary>
        /// <remarks>THE SAME INSTANCE <see cref="GpioController"/> drives, resolved through the
        /// board's table rather than constructed here. One block has one driver, and handing out a
        /// second one over the same registers reads as working while the facade talks to the
        /// first -- see <see cref="Lamella.Hardware.Buses.ResolveSpi"/> for the full argument.</remarks>
        public GpioDriver CreateGpioDriver()
        {
            return Buses.ResolveGpio();
        }
    }
}
