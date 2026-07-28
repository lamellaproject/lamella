// The descriptor a SAM3X UART driver consumes: one binding's resolved values, exactly the consts a
namespace Lamella.Boards
{
    public sealed class Sam3xUartBinding
    {
        /// <summary>The bound UART instance's base address.</summary>
        public readonly uint UartBase;
        /// <summary>The instance's peripheral id -- on this part both the PMC gate bit and the
        /// NVIC line.</summary>
        public readonly uint Pid;
        /// <summary>The resolved PMC Peripheral Clock Enable register the gate bit lives in.</summary>
        public readonly uint PmcPcerReg;
        /// <summary>The instance's PMC clock-gate bit, as a mask (1 &lt;&lt; pid).</summary>
        public readonly uint PmcPcerMask;
        /// <summary>The resolved PIO Disable register of the port carrying the TX/RX pins --
        /// writing the mask there hands both lines to the peripheral.</summary>
        public readonly uint PioPdrReg;
        /// <summary>The resolved PIO peripheral-AB Select register of that port.</summary>
        public readonly uint PioAbsrReg;
        /// <summary>The TX|RX line mask within the port.</summary>
        public readonly uint PioMask;
        /// <summary>The binding's peripheral function as its ABSR value (0 = A, 1 = B). A is the
        /// reset default, so a function-A binding needs no ABSR write at all.</summary>
        public readonly uint PioFunc;
        /// <summary>The routed master-clock rate under the board's default plan -- the rate the
        /// baud generator divides (baud = MCK / (16 * CD)).</summary>
        public readonly uint MckHz;
        /// <summary>The BRGR clock divisor (CD) for the binding's wire rate under the board's
        /// default clock plan (plan-derived).</summary>
        public readonly uint BaudDivisor;

        public Sam3xUartBinding(uint uartBase, uint pid, uint pmcPcerReg, uint pmcPcerMask,
            uint pioPdrReg, uint pioAbsrReg, uint pioMask, uint pioFunc,
            uint mckHz, uint baudDivisor)
        {
            UartBase = uartBase;
            Pid = pid;
            PmcPcerReg = pmcPcerReg;
            PmcPcerMask = pmcPcerMask;
            PioPdrReg = pioPdrReg;
            PioAbsrReg = pioAbsrReg;
            PioMask = pioMask;
            PioFunc = pioFunc;
            MckHz = mckHz;
            BaudDivisor = baudDivisor;
        }
    }
}
