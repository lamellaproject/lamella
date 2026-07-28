// Lamella.Boards.ArduinoDue -- the Arduino Due (ATSAM3X8E), the first sam3x-family board. Board
using Lamella.Generated;

namespace Lamella.Boards
{
    public sealed class ArduinoDue
    {
        /// <summary>The wire identity this board advertises (lamella_wire::board_model).</summary>
        public static readonly int BoardModel = ArduinoDueBindings.BOARD_MODEL;

        /// <summary>The routed master clock under the board's default plan, in Hz -- also the
        /// part's documented maximum.</summary>
        public static readonly uint MasterClockHz = ArduinoDueBindings.VCP_MCK_HZ;

        /// <summary>The programming-port UART (PA9 TX / PA8 RX, peripheral A, 115200-8N1 under the
        /// plla-84mhz plan), ready for <c>Init()</c>.</summary>
        public Sam3xUart CreateVcpUart()
        {
            return new Sam3xUart(new Sam3xUartBinding(
                ArduinoDueBindings.VCP_BASE,
                ArduinoDueBindings.VCP_PID,
                ArduinoDueBindings.VCP_PMC_PCER_REG,
                ArduinoDueBindings.VCP_PMC_PCER_MASK,
                ArduinoDueBindings.VCP_PIO_PDR_REG,
                ArduinoDueBindings.VCP_PIO_ABSR_REG,
                ArduinoDueBindings.VCP_PIO_MASK,
                ArduinoDueBindings.VCP_PIO_FUNC,
                ArduinoDueBindings.VCP_MCK_HZ,
                ArduinoDueBindings.VCP_BRGR_CD_115200_PLLA_84MHZ));
        }
    }
}
