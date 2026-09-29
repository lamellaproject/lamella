// The descriptor an RP2350 PWM driver consumes: one slice of the PWM block, its clock, and every
// output a board routes from it to a pad.
namespace Lamella.Boards
{
    /// <summary>One output of a slice that a board routes to a pad: which of the slice's two outputs
    /// it is, and the pad's two registers.</summary>
    public sealed class Rp2350PwmOutput
    {
        /// <summary>0 for the slice's output A and 1 for B: the half of the slice's CC register that
        /// sets its level, and the channel number <c>PwmChannel.Create</c> names it by.</summary>
        public readonly int Index;
        /// <summary>The pad's IO_BANK0 CTRL register, whose function select routes it.</summary>
        public readonly uint IoCtrl;
        /// <summary>The pad's PADS_BANK0 register.</summary>
        public readonly uint Pads;

        public Rp2350PwmOutput(uint index, uint ioCtrl, uint pads)
        {
            Index = (int)index;
            IoCtrl = ioCtrl;
            Pads = pads;
        }
    }

    public sealed class Rp2350PwmBinding
    {
        /// <summary>The PWM block's base address.</summary>
        public readonly uint PwmBase;
        /// <summary>The RESETS release set: the PWM block and both IO banks.</summary>
        public readonly uint ResetMask;
        /// <summary>The slice, from 0 to 11: the index into the block's per-slice registers.</summary>
        public readonly uint Slice;
        /// <summary>The function-select value routing a pad to the PWM block.</summary>
        public readonly uint Funcsel;
        /// <summary>clk_sys under the board plan, in hertz: the rate the slice's divider divides.</summary>
        public readonly uint ClkSysHz;

        private readonly Rp2350PwmOutput[] _outputs;

        public Rp2350PwmBinding(uint pwmBase, uint resetMask, uint slice, uint funcsel, uint clkSysHz,
            Rp2350PwmOutput[] outputs)
        {
            if ((object)outputs == null)
            {
                throw new System.ArgumentNullException("outputs");
            }
            PwmBase = pwmBase;
            ResetMask = resetMask;
            Slice = slice;
            Funcsel = funcsel;
            ClkSysHz = clkSysHz;
            _outputs = new Rp2350PwmOutput[2];
            for (int i = 0; i < outputs.Length; i++)
            {
                Rp2350PwmOutput output = outputs[i];
                if ((object)output == null)
                {
                    throw new System.ArgumentNullException("outputs");
                }
                if (output.Index < 0 || output.Index > 1)
                {
                    throw new System.ArgumentOutOfRangeException("outputs");
                }
                if ((object)_outputs[output.Index] != null)
                {
                    throw new System.ArgumentException("two outputs of the slice have index " + output.Index);
                }
                _outputs[output.Index] = output;
            }
        }

        /// <summary>The output routed as channel <paramref name="channel"/>, 0 for A and 1 for B, or
        /// null when the board routes none there.</summary>
        public Rp2350PwmOutput Output(int channel)
        {
            if (channel < 0 || channel > 1)
            {
                return null;
            }
            return _outputs[channel];
        }
    }
}
