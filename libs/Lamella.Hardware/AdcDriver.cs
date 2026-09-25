// Lamella.Hardware -- the ADC chip-driver seam.
namespace Lamella.Hardware
{
    /// <summary>Base class for ADC drivers: convert analog inputs to digital counts.</summary>
    public abstract class AdcDriver : System.IDisposable
    {
        /// <summary>The span of the chip's channel numbers: every channel is numbered below it.</summary>
        /// <remarks>On a converter whose channel numbers are its multiplexer's input codes, the
        /// codes can have gaps, so a number below this span is not necessarily a channel;
        /// <see cref="IsChannelSupported"/> says which are.</remarks>
        public abstract int ChannelCount { get; }

        /// <summary>Whether <paramref name="channel"/> is a channel this chip converts. Answers
        /// false rather than throwing, so it can be used to test a channel number.</summary>
        /// <remarks>The default admits every number from 0 to <see cref="ChannelCount"/> - 1,
        /// which is right for a converter whose channels are numbered without gaps; a driver
        /// whose channel numbers have gaps overrides it.</remarks>
        public virtual bool IsChannelSupported(int channel)
        {
            return channel >= 0 && channel < ChannelCount;
        }

        /// <summary>The width, in bits, of a conversion result.</summary>
        public abstract int ResolutionInBits { get; }

        /// <summary>The lowest count a conversion can report.</summary>
        public abstract int MinValue { get; }

        /// <summary>The highest count a conversion can report.</summary>
        public abstract int MaxValue { get; }

        /// <summary>Whether the chip can convert in <paramref name="mode"/>.</summary>
        public abstract bool IsChannelModeSupported(AdcChannelMode mode);

        /// <summary>Puts the chip into <paramref name="mode"/> for subsequent
        /// conversions.</summary>
        public abstract void SetChannelMode(AdcChannelMode mode);

        /// <summary>Claims a channel and runs its per-channel enable steps (pad-to-analog
        /// prep for pin-backed channels, bias enables for internal sources).</summary>
        public abstract void OpenChannel(int channel);

        /// <summary>Releases a channel claimed by <see cref="OpenChannel"/>. Releasing a channel
        /// that is not claimed does nothing, so a release may safely be repeated.</summary>
        public abstract void CloseChannel(int channel);

        /// <summary>Performs one conversion on a channel and returns the hardware count, or a
        /// negative status when the converter could not produce one.</summary>
        /// <remarks>A count is never negative, so a negative return cannot be mistaken for a
        /// reading: -3 means the conversion failed. A driver returns it rather than a sample its
        /// converter flagged as undefined, and every surface over this seam raises it as an
        /// error instead of passing it on as a value.</remarks>
        public abstract int ReadValue(int channel);

        public void Dispose()
        {
            Dispose(true);
        }

        protected virtual void Dispose(bool disposing)
        {
        }
    }
}
