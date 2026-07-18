// Lamella.Boards.Pico -- the original Raspberry Pi Pico / Pico H (RP2040) board-support
using Lamella.Generated;

namespace Lamella.Boards
{
    public sealed class Pico
    {
        /// <summary>The wire identity this board advertises (lamella_wire::board_model).</summary>
        public static readonly int BoardModel = PicoBindings.BOARD_MODEL;

        /// <summary>The UART0 FIFO depth, single-sourced from the block layout.</summary>
        public static readonly int UartFifoDepth = (int)Rp2040UartLayout.FIFO_DEPTH;

        /// <summary>UART0 on GP0 (TX, header pin 1) / GP1 (RX, pin 2), clocked from the
        /// crystal-exact clk_peri under the xosc-12mhz plan, ready for <c>Init(baud)</c>.</summary>
        public Rp2040Uart CreateUart()
        {
            return new Rp2040Uart(new Rp2040UartBinding(
                PicoBindings.UART0_BASE,
                PicoBindings.UART0_RESET_MASK,
                PicoBindings.UART0_IO_TX_CTRL,
                PicoBindings.UART0_IO_RX_CTRL,
                PicoBindings.UART0_FUNCSEL,
                PicoBindings.UART0_CLK_PERI_HZ));
        }
    }
}
