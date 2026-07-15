// Lamella.Boards.Esp32C6 -- the Espressif ESP32-C6 (RISC-V RV32IMAC) board-support package.
using System.Device.Gpio;
using Lamella.Generated;

namespace Lamella.Boards
{
    public sealed class Esp32C6
    {
        /// <summary>The UART0 TX/RX FIFO depth, single-sourced (runtime read).</summary>
        public static readonly int UartFifoDepth = (int)Esp32c6UartFacts.FIFO_DEPTH;

        /// <summary>UART0 (native U0TXD/U0RXD), initialized for <paramref name="baud"/> 8N1.</summary>
        public Esp32C6Uart CreateUart(int baud)
        {
            Esp32C6Uart uart = new Esp32C6Uart();
            uart.Init(baud);
            return uart;
        }

        /// <summary>The GPIO block (RGB LED on GPIO8 via RMT on the DevKit; general IO).</summary>
        public GpioDriver CreateGpioDriver()
        {
            return new Esp32C6GpioDriver();
        }
    }
}
