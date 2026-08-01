// Lamella.Boards.Microchip.Samd21Xpro -- the plain SAM D21 Xplained Pro (ATSAMD21J18A). Its EDBG VCP is
using Lamella.Generated;

namespace Lamella.Boards.Microchip
{
    public sealed class Samd21Xpro
    {
        /// <summary>The wire identity this board advertises (lamella_wire::board_model).</summary>
        public static readonly int BoardModel = Samd21XproBindings.BOARD_MODEL;

        /// <summary>The EDBG virtual-COM UART (SERCOM3, PA22 TX / PA23 RX, 115200-8N1 under
        /// the osc8m-8mhz plan), ready for <c>Init()</c>.</summary>
        public Samd21Uart CreateVcpUart()
        {
            return new Samd21Uart(new Samd21SercomUsartBinding(
                Samd21XproBindings.VCP_SERCOM_BASE,
                Samd21XproBindings.VCP_GCLK_CLKCTRL_VALUE,
                Samd21XproBindings.VCP_APBC_MASK,
                Samd21XproBindings.VCP_PMUX_REG,
                Samd21XproBindings.VCP_PMUX_PAIR,
                Samd21XproBindings.VCP_PINCFG_TX_REG,
                Samd21XproBindings.VCP_PINCFG_RX_REG,
                Samd21XproBindings.VCP_TXPO,
                Samd21XproBindings.VCP_RXPO,
                Samd21XproBindings.VCP_BAUD_115200_OSC8M_8MHZ));
        }
    }
}
