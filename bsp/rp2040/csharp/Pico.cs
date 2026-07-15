// Lamella.Boards.Pico -- the original Raspberry Pi Pico / Pico W (RP2040) board-support package.
using Lamella.Generated;

namespace Lamella.Boards
{
    public sealed class Pico
    {
        /// <summary>The UART0 FIFO depth, single-sourced (runtime read).</summary>
        public static readonly int UartFifoDepth = (int)Rp2040UartFacts.FIFO_DEPTH;

        /// <summary>UART0 on GP0 (TX) / GP1 (RX), initialized for <paramref name="baud"/> 8N1
        /// off the crystal-exact clk_peri.</summary>
        public Rp2040Uart CreateUart(int baud)
        {
            Rp2040Uart uart = new Rp2040Uart();
            uart.Init(baud);
            return uart;
        }
    }
}
