// Lamella.Boards.Microchip.Samw25Xpro -- the SAM W25 Xplained Pro (ATSAMW25 module: a SAMD21G18A host +
using System.Device.Gpio;
using Lamella.Generated;
using Lamella.Hardware;

namespace Lamella.Boards.Microchip
{
    public sealed class Samw25Xpro
    {
        /// <summary>The wire identity this board advertises (lamella_wire::product_model).</summary>
        public static readonly int BoardModel = Samw25XproBindings.BOARD_MODEL;

        /// <summary>Binds this board's GPIO block to the driver table, so a program writes plain
        /// dotnet/iot -- <c>new GpioController()</c> -- and never names a Lamella type. Touching
        /// <see cref="Samw25Xpro"/> at all is what arms it, which is why a program constructs the
        /// board first.</summary>
        /// <remarks>A TYPE INITIALIZER rather than the instance constructor, for the reason
        /// <see cref="Lamella.Hardware.Buses.BindGpio"/> documents: the table refuses a second bind
        /// of the same kind rather than replacing it, and this class is instantiable and routinely
        /// constructed as a temporary. The language runs a type initializer once per program, so
        /// idempotence costs nothing and the table keeps its throw as a genuine-error detector.
        /// The bound value is a FACTORY, not a driver, so a program that never touches GPIO never
        /// constructs one.</remarks>
        static Samw25Xpro()
        {
            Buses.BindGpio(new GpioDriverFactory(MakeGpio));
        }

        private static GpioDriver MakeGpio() { return new Samd21GpioDriver(); }

        /// <summary>The family PORT driver this board bound, over every pin on the part.</summary>
        /// <remarks>THE SAME INSTANCE <see cref="GpioController"/> drives. One block has one
        /// driver, and handing out a second one over the same registers reads as working while
        /// the facade talks to the first -- see
        /// <see cref="Lamella.Hardware.Buses.ResolveSpi"/> for the full argument.</remarks>
        public GpioDriver CreateGpioDriver()
        {
            return Buses.ResolveGpio();
        }

        /// <summary>The EDBG virtual-COM UART (SERCOM4, PB10 TX / PB11 RX, 115200-8N1 under
        /// the osc8m-8mhz plan), ready for <c>Init()</c>.</summary>
        public Samd21Uart CreateVcpUart()
        {
            return new Samd21Uart(new Samd21SercomUsartBinding(
                Samw25XproBindings.VCP_SERCOM_BASE,
                Samw25XproBindings.VCP_GCLK_CLKCTRL_VALUE,
                Samw25XproBindings.VCP_APBC_MASK,
                Samw25XproBindings.VCP_PMUX_REG,
                Samw25XproBindings.VCP_PMUX_PAIR,
                Samw25XproBindings.VCP_PINCFG_TX_REG,
                Samw25XproBindings.VCP_PINCFG_RX_REG,
                Samw25XproBindings.VCP_TXPO,
                Samw25XproBindings.VCP_RXPO,
                Samw25XproBindings.VCP_BAUD_115200_OSC8M_8MHZ));
        }
    }
}
