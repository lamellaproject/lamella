// Lamella.Boards.Arduino.Zero -- the Arduino Zero (ATSAMD21G18A + on-board EDBG). Its EDBG VCP
using System.Device.Gpio;
using Lamella.Generated;
using Lamella.Hardware;

namespace Lamella.Boards.Arduino
{
    public sealed class Zero
    {
        /// <summary>The wire identity this board advertises (lamella_wire::product_model).</summary>
        public static readonly int BoardModel = ZeroBindings.BOARD_MODEL;

        /// <summary>Binds this board's GPIO block to the driver table, so a program writes plain
        /// dotnet/iot -- <c>new GpioController()</c> -- and never names a Lamella type. Touching
        /// <see cref="Zero"/> at all is what arms it, which is why a program constructs the
        /// board first.</summary>
        /// <remarks>A TYPE INITIALIZER rather than the instance constructor, for the reason
        /// <see cref="Lamella.Hardware.Buses.BindGpio"/> documents: the table refuses a second bind
        /// of the same kind rather than replacing it, and this class is instantiable and routinely
        /// constructed as a temporary. The language runs a type initializer once per program, so
        /// idempotence costs nothing and the table keeps its throw as a genuine-error detector.
        /// The bound value is a FACTORY, not a driver, so a program that never touches GPIO never
        /// constructs one.</remarks>
        static Zero()
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

        /// <summary>The EDBG virtual-COM UART (SERCOM5, PB22 TX / PB23 RX, 115200-8N1 under
        /// the osc8m-8mhz plan), ready for <c>Init()</c>.</summary>
        public Samd21Uart CreateVcpUart()
        {
            return new Samd21Uart(new Samd21SercomUsartBinding(
                ZeroBindings.VCP_SERCOM_BASE,
                ZeroBindings.VCP_GCLK_CLKCTRL_VALUE,
                ZeroBindings.VCP_APBC_MASK,
                ZeroBindings.VCP_PMUX_REG,
                ZeroBindings.VCP_PMUX_PAIR,
                ZeroBindings.VCP_PINCFG_TX_REG,
                ZeroBindings.VCP_PINCFG_RX_REG,
                ZeroBindings.VCP_TXPO,
                ZeroBindings.VCP_RXPO,
                ZeroBindings.VCP_BAUD_115200_OSC8M_8MHZ));
        }

        /// <summary>The board's dedicated SDA/SCL header pins beside AREF (D20/D21 = PA22/PA23
        /// on SERCOM3), as an I2C master descriptor. The bus SPEED is not here: it is a runtime
        /// <c>Configure</c> choice derived on the device from the core-clock rate.</summary>
        public Samd21SercomI2cBinding HeaderI2cBinding()
        {
            return new Samd21SercomI2cBinding(
                ZeroBindings.HEADER_I2C_SERCOM_BASE,
                ZeroBindings.HEADER_I2C_GCLK_CLKCTRL_VALUE,
                ZeroBindings.HEADER_I2C_APBC_MASK,
                ZeroBindings.HEADER_I2C_PMUX_REG,
                ZeroBindings.HEADER_I2C_PMUX_PAIR,
                ZeroBindings.HEADER_I2C_PINCFG_SDA_REG,
                ZeroBindings.HEADER_I2C_PINCFG_SCL_REG,
                ZeroBindings.HEADER_I2C_CORE_CLOCK_HZ);
        }
    }
}
