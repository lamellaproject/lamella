// Lamella.Boards.Microchip.Samw25Xpro -- the SAM W25 Xplained Pro (ATSAMW25 module: a SAMD21G18A host +
using Lamella.Generated;

namespace Lamella.Boards.Microchip
{
    public sealed class Samw25Xpro
    {
        /// <summary>The wire identity this board advertises (lamella_wire::board_model).</summary>
        public static readonly int BoardModel = Samw25XproBindings.BOARD_MODEL;

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
