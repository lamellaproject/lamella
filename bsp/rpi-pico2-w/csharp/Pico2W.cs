// Lamella.Boards.RaspberryPi.Pico2W -- the Raspberry Pi Pico 2 W (RP2350A + an Infineon CYW43439).
using System.Device.Gpio;
using Lamella.Generated;
using Lamella.Hardware;

namespace Lamella.Boards.RaspberryPi
{
    public sealed class Pico2W
    {
        /// <summary>The wire identity this board advertises (lamella_wire::board_model).</summary>
        public static readonly int BoardModel = Pico2WBindings.BOARD_MODEL;

        /// <summary>The pins the CYW43439 owns, as a mask over bank 0 -- WL_REG_ON, the shared
        /// data/IRQ line, the chip select and the clock. Composed from the generated per-line masks
        /// rather than written out, so a line moved in board.toml moves here.</summary>
        public static readonly uint RadioPins =
            Pico2WBindings.CYW43439_WL_REG_ON_MASK
            | Pico2WBindings.CYW43439_DATA_MASK
            | Pico2WBindings.CYW43439_CS_MASK
            | Pico2WBindings.CYW43439_CLK_MASK;

        /// <summary>Binds this board's GPIO block to the driver table, so a program writes plain
        /// dotnet/iot -- <c>new GpioController()</c> -- and never names a Lamella type. Touching
        /// <see cref="Pico2W"/> at all is what arms it, which is why a program constructs the board
        /// first.</summary>
        /// <remarks>A TYPE INITIALIZER rather than the instance constructor, for the reason
        /// <see cref="Lamella.Hardware.Buses.BindGpio"/> documents: the table refuses a second bind
        /// of the same kind rather than replacing it, and this class is instantiable and routinely
        /// constructed as a temporary. The language runs a type initializer once per program, so
        /// idempotence costs nothing and the table keeps its throw as a genuine-error detector.
        /// The bound value is a FACTORY, not a driver, so a program that never touches GPIO never
        /// constructs one.</remarks>
        static Pico2W()
        {
            Buses.BindGpio(new GpioDriverFactory(MakeGpio));
        }

        private static GpioDriver MakeGpio() { return new Rp2350GpioDriver(RadioPins); }

        /// <summary>The family SIO/pad driver this board bound, over bank 0 less the four lines in
        /// <see cref="RadioPins"/>.</summary>
        /// <remarks>THE SAME INSTANCE <see cref="GpioController"/> drives. One block has one
        /// driver, and handing out a second one over the same registers reads as working while the
        /// facade talks to the first -- see <see cref="Lamella.Hardware.Buses.ResolveSpi"/> for the
        /// full argument.</remarks>
        public GpioDriver CreateGpioDriver()
        {
            return Buses.ResolveGpio();
        }

        /// <summary>A GPIO controller over the RP2350 SIO/pad block. The radio's four lines refuse
        /// <c>SetPinMode</c>.</summary>
        public GpioController CreateGpioController()
        {
            return new GpioController();
        }

        /// <summary>The `uart0` binding descriptor (GP0 TX / GP1 RX, crystal-exact clk_peri).</summary>
        public Rp2350UartBinding CreateUartBinding()
        {
            return new Rp2350UartBinding(
                Pico2WBindings.UART0_BASE,
                Pico2WBindings.UART0_RESET_MASK,
                Pico2WBindings.UART0_IO_TX_CTRL,
                Pico2WBindings.UART0_IO_RX_CTRL,
                Pico2WBindings.UART0_PADS_TX,
                Pico2WBindings.UART0_PADS_RX,
                Pico2WBindings.UART0_FUNCSEL,
                Pico2WBindings.UART0_CLK_PERI_HZ);
        }

        /// <summary>UART0 on GP0 (TX, header pin 1) / GP1 (RX, pin 2), ready for
        /// <c>Init(baud)</c>.</summary>
        public Rp2350Uart CreateUart()
        {
            return new Rp2350Uart(CreateUartBinding());
        }
    }
}
