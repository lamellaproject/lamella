// The descriptor an RP2040 PL011-UART driver consumes: one binding's resolved values, exactly
namespace Lamella.Boards
{
    public sealed class Rp2040UartBinding
    {
        /// <summary>The bound PL011 instance's base address.</summary>
        public readonly uint UartBase;
        /// <summary>The RESETS release set for this binding (the uart instance plus both IO
        /// banks), written through the write-1-to-clear alias and polled on RESET_DONE.</summary>
        public readonly uint ResetMask;
        /// <summary>The resolved IO_BANK0 GPIO CTRL address of the TX pin.</summary>
        public readonly uint IoTxCtrl;
        /// <summary>The resolved IO_BANK0 GPIO CTRL address of the RX pin.</summary>
        public readonly uint IoRxCtrl;
        /// <summary>The function-select value routing both pins to the bound instance.</summary>
        public readonly uint Funcsel;
        /// <summary>The UART clock rate under the board's default plan (clk_peri; the PL011
        /// divisor derives from it at Init).</summary>
        public readonly uint ClkPeriHz;

        public Rp2040UartBinding(uint uartBase, uint resetMask, uint ioTxCtrl, uint ioRxCtrl,
            uint funcsel, uint clkPeriHz)
        {
            UartBase = uartBase;
            ResetMask = resetMask;
            IoTxCtrl = ioTxCtrl;
            IoRxCtrl = ioRxCtrl;
            Funcsel = funcsel;
            ClkPeriHz = clkPeriHz;
        }
    }
}
