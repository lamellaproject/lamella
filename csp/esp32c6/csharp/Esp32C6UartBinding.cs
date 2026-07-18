// The descriptor an ESP32-C6 HP-UART driver consumes: one binding's resolved values, exactly
namespace Lamella.Boards
{
    public sealed class Esp32C6UartBinding
    {
        /// <summary>The bound HP-UART instance's base address.</summary>
        public readonly uint UartBase;
        /// <summary>The instance's PCR slot register: module bus clock + reset.</summary>
        public readonly uint PcrConf;
        /// <summary>The instance's PCR slot register: function-clock source + coarse divider.</summary>
        public readonly uint PcrSclkConf;
        /// <summary>The resolved IO_MUX register address of the TX pin.</summary>
        public readonly uint IoMuxTx;
        /// <summary>The resolved IO_MUX register address of the RX pin.</summary>
        public readonly uint IoMuxRx;
        /// <summary>The IO_MUX function (MCU_SEL) routing both pins to the bound instance
        /// (0 = each pin's native signal; no GPIO matrix involved).</summary>
        public readonly uint McuSel;
        /// <summary>The UART function-clock rate under the board's default plan (the CLKDIV
        /// divisor derives from it at Init).</summary>
        public readonly uint SclkHz;

        public Esp32C6UartBinding(uint uartBase, uint pcrConf, uint pcrSclkConf, uint ioMuxTx,
            uint ioMuxRx, uint mcuSel, uint sclkHz)
        {
            UartBase = uartBase;
            PcrConf = pcrConf;
            PcrSclkConf = pcrSclkConf;
            IoMuxTx = ioMuxTx;
            IoMuxRx = ioMuxRx;
            McuSel = mcuSel;
            SclkHz = sclkHz;
        }
    }
}
