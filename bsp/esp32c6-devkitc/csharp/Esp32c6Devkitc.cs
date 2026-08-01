// Lamella.Boards.Espressif.Esp32c6Devkitc -- the Espressif ESP32-C6-DevKitC-1 (RISC-V RV32IMAC) board
using System.Device.Gpio;
using Lamella.Generated;

namespace Lamella.Boards.Espressif
{
    public sealed class Esp32c6Devkitc
    {
        /// <summary>The wire identity this board advertises (lamella_wire::board_model).</summary>
        public static readonly int BoardModel = Esp32c6DevkitcBindings.BOARD_MODEL;

        /// <summary>The UART TX/RX FIFO depth, single-sourced from the block layout.</summary>
        public static readonly int UartFifoDepth = (int)Esp32c6UartLayout.FIFO_DEPTH;

        /// <summary>UART0 on its native IO_MUX pins (TX GPIO16 / RX GPIO17 -- the ROM console,
        /// routed to the on-board USB-UART bridge), ready for <c>Init(baud)</c>.</summary>
        public Esp32C6Uart CreateUart()
        {
            return new Esp32C6Uart(new Esp32C6UartBinding(
                Esp32c6DevkitcBindings.UART0_BASE,
                Esp32c6DevkitcBindings.UART0_PCR_CONF,
                Esp32c6DevkitcBindings.UART0_PCR_SCLK_CONF,
                Esp32c6DevkitcBindings.UART0_IO_MUX_TX,
                Esp32c6DevkitcBindings.UART0_IO_MUX_RX,
                Esp32c6DevkitcBindings.UART0_MCU_SEL,
                Esp32c6DevkitcBindings.UART0_SCLK_HZ));
        }

        /// <summary>The GPIO block (RGB LED on GPIO8 via RMT on the DevKit; general IO).</summary>
        public GpioDriver CreateGpioDriver()
        {
            return new Esp32C6GpioDriver();
        }
    }
}
