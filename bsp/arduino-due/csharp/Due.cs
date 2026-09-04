// Lamella.Boards.Arduino.Due -- the Arduino Due (ATSAM3X8E), the first sam3x-family board. Board
using Lamella.Generated;

namespace Lamella.Boards.Arduino
{
    public sealed class Due
    {
        /// <summary>The wire identity this board advertises (lamella_wire::product_model).</summary>
        public static readonly int BoardModel = DueBindings.BOARD_MODEL;

        /// <summary>The routed master clock under the board's default plan, in Hz -- also the
        /// part's documented maximum.</summary>
        public static readonly uint MasterClockHz = DueBindings.VCP_MCK_HZ;

        /// <summary>The programming-port UART (PA9 TX / PA8 RX, peripheral A, 115200-8N1 under the
        /// plla-84mhz plan), ready for <c>Init()</c>.</summary>
        public Sam3xUart CreateVcpUart()
        {
            return new Sam3xUart(new Sam3xUartBinding(
                DueBindings.VCP_BASE,
                DueBindings.VCP_PID,
                DueBindings.VCP_PMC_PCER_REG,
                DueBindings.VCP_PMC_PCER_MASK,
                DueBindings.VCP_PIO_PDR_REG,
                DueBindings.VCP_PIO_ABSR_REG,
                DueBindings.VCP_PIO_MASK,
                DueBindings.VCP_PIO_FUNC,
                DueBindings.VCP_MCK_HZ,
                DueBindings.VCP_BRGR_CD_115200_PLLA_84MHZ));
        }
    }
}
