// Lamella.Boards.NucleoL476rg -- the ST NUCLEO-L476RG (STM32L476RG, Cortex-M4F). Its ST-LINK
using Lamella.Generated;

namespace Lamella.Boards
{
    public sealed class NucleoL476rg
    {
        /// <summary>The wire identity this board advertises (lamella_wire::board_model).</summary>
        public static readonly int BoardModel = NucleoL476rgBindings.BOARD_MODEL;

        /// <summary>The ST-LINK virtual-COM UART (USART2, PA2 TX / PA3 RX at AF7, 115200-8N1
        /// under the board's MSI 4 MHz reset plan), ready for <c>Init()</c>.</summary>
        public Stm32l476Uart CreateVcpUart()
        {
            return new Stm32l476Uart(new Stm32l476UsartBinding(
                NucleoL476rgBindings.VCP_BASE,
                NucleoL476rgBindings.VCP_RCC_EN_REG,
                NucleoL476rgBindings.VCP_RCC_EN_MASK,
                NucleoL476rgBindings.VCP_PORT_RCC_EN_REG,
                NucleoL476rgBindings.VCP_PORT_RCC_EN_MASK,
                NucleoL476rgBindings.VCP_MODER_REG,
                NucleoL476rgBindings.VCP_MODER_MASK,
                NucleoL476rgBindings.VCP_MODER_VALUE,
                NucleoL476rgBindings.VCP_AFRL_REG,
                NucleoL476rgBindings.VCP_AFRL_MASK,
                NucleoL476rgBindings.VCP_AFRL_VALUE,
                NucleoL476rgBindings.VCP_BRR_115200_MSI_4MHZ));
        }
    }
}
