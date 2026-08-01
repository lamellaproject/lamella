// Lamella.Boards.Arduino.Zero -- the Arduino Zero (ATSAMD21G18A + on-board EDBG). Its EDBG VCP
using Lamella.Generated;

namespace Lamella.Boards.Arduino
{
    public sealed class Zero
    {
        /// <summary>The wire identity this board advertises (lamella_wire::board_model).</summary>
        public static readonly int BoardModel = ZeroBindings.BOARD_MODEL;

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
