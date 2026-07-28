// Lamella.Boards.ArduinoZero -- the Arduino Zero (ATSAMD21G18A + on-board EDBG). Its EDBG VCP
using Lamella.Generated;

namespace Lamella.Boards
{
    public sealed class ArduinoZero
    {
        /// <summary>The wire identity this board advertises (lamella_wire::board_model).</summary>
        public static readonly int BoardModel = ArduinoZeroBindings.BOARD_MODEL;

        /// <summary>The EDBG virtual-COM UART (SERCOM5, PB22 TX / PB23 RX, 115200-8N1 under
        /// the osc8m-8mhz plan), ready for <c>Init()</c>.</summary>
        public Samd21Uart CreateVcpUart()
        {
            return new Samd21Uart(new Samd21SercomUsartBinding(
                ArduinoZeroBindings.VCP_SERCOM_BASE,
                ArduinoZeroBindings.VCP_GCLK_CLKCTRL_VALUE,
                ArduinoZeroBindings.VCP_APBC_MASK,
                ArduinoZeroBindings.VCP_PMUX_REG,
                ArduinoZeroBindings.VCP_PMUX_PAIR,
                ArduinoZeroBindings.VCP_PINCFG_TX_REG,
                ArduinoZeroBindings.VCP_PINCFG_RX_REG,
                ArduinoZeroBindings.VCP_TXPO,
                ArduinoZeroBindings.VCP_RXPO,
                ArduinoZeroBindings.VCP_BAUD_115200_OSC8M_8MHZ));
        }

        /// <summary>The board's dedicated SDA/SCL header pins beside AREF (D20/D21 = PA22/PA23
        /// on SERCOM3), as an I2C master descriptor. The bus SPEED is not here: it is a runtime
        /// <c>Configure</c> choice derived on the device from the core-clock rate.</summary>
        public Samd21SercomI2cBinding HeaderI2cBinding()
        {
            return new Samd21SercomI2cBinding(
                ArduinoZeroBindings.HEADER_I2C_SERCOM_BASE,
                ArduinoZeroBindings.HEADER_I2C_GCLK_CLKCTRL_VALUE,
                ArduinoZeroBindings.HEADER_I2C_APBC_MASK,
                ArduinoZeroBindings.HEADER_I2C_PMUX_REG,
                ArduinoZeroBindings.HEADER_I2C_PMUX_PAIR,
                ArduinoZeroBindings.HEADER_I2C_PINCFG_SDA_REG,
                ArduinoZeroBindings.HEADER_I2C_PINCFG_SCL_REG,
                ArduinoZeroBindings.HEADER_I2C_CORE_CLOCK_HZ);
        }
    }
}
