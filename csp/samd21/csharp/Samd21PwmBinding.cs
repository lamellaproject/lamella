// The descriptor a SAMD21 PWM driver consumes: one timer/counter's resolved clocking, and every
// output pad a board wired to it.
namespace Lamella.Boards
{
    /// <summary>One output a board wired to a counter: the compare channel that drives it, and
    /// where its pad's multiplexer nibble and pin configuration byte are.</summary>
    public sealed class Samd21PwmOutput
    {
        /// <summary>The compare channel that drives the output: n for the counter's waveform
        /// output WO[n].</summary>
        public readonly int CompareChannel;
        /// <summary>The PORT PMUX byte covering the pad.</summary>
        public readonly uint PmuxReg;
        /// <summary>The pad's nibble within that byte: 0 for an even pad, 4 for an odd one.</summary>
        public readonly uint PmuxShift;
        /// <summary>The PORT PINCFG byte of the pad.</summary>
        public readonly uint PincfgReg;

        public Samd21PwmOutput(int compareChannel, uint pmuxReg, uint pmuxShift, uint pincfgReg)
        {
            CompareChannel = compareChannel;
            PmuxReg = pmuxReg;
            PmuxShift = pmuxShift;
            PincfgReg = pincfgReg;
        }
    }

    public sealed class Samd21PwmBinding
    {
        /// <summary>The counter's base address: a TC's or a TCC's.</summary>
        public readonly uint CounterBase;
        /// <summary>The composed GCLK.CLKCTRL word routing the counter's clock (ID | generator |
        /// CLKEN), from the instance row and the board plan.</summary>
        public readonly uint GclkClkctrlValue;
        /// <summary>The counter's PM.APBCMASK gate bit, as a mask.</summary>
        public readonly uint ApbcMask;
        /// <summary>The rate of the counter's generic clock under the board plan, in hertz.</summary>
        public readonly uint CoreClockHz;
        /// <summary>How many bits the counter counts in, which bounds its period.</summary>
        public readonly uint CounterBits;
        /// <summary>The PMUX nibble value selecting the counter's function on the pads.</summary>
        public readonly uint PmuxFunc;

        private readonly Samd21PwmOutput[] _outputs;

        public Samd21PwmBinding(uint counterBase, uint gclkClkctrlValue, uint apbcMask, uint coreClockHz,
            uint counterBits, uint pmuxFunc, Samd21PwmOutput[] outputs)
        {
            if ((object)outputs == null)
            {
                throw new System.ArgumentNullException("outputs");
            }
            CounterBase = counterBase;
            GclkClkctrlValue = gclkClkctrlValue;
            ApbcMask = apbcMask;
            CoreClockHz = coreClockHz;
            CounterBits = counterBits;
            PmuxFunc = pmuxFunc;
            // A copy, so the table a driver was built over cannot change under it.
            _outputs = new Samd21PwmOutput[outputs.Length];
            for (int i = 0; i < outputs.Length; i++)
            {
                _outputs[i] = outputs[i];
            }
        }

        /// <summary>How many outputs the board wired to the counter: the channels of its PWM chip.</summary>
        public int OutputCount { get { return _outputs.Length; } }

        /// <summary>The wired output that is channel <paramref name="index"/>, from 0 to
        /// <see cref="OutputCount"/> - 1.</summary>
        public Samd21PwmOutput Output(int index)
        {
            return _outputs[index];
        }
    }
}
