// Lamella.Hardware -- the ADC chip-driver seam, in the nanoFramework.System.Device.Adc assembly.
using System.Device.Adc;

namespace Lamella.Hardware
{
    /// <summary>Base class for ADC drivers: convert analog inputs to digital counts.</summary>
    public abstract class AdcDriver : System.IDisposable
    {
        /// <summary>The number of channels the chip converts.</summary>
        public abstract int ChannelCount { get; }

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

        /// <summary>Releases a channel claimed by <see cref="OpenChannel"/>.</summary>
        public abstract void CloseChannel(int channel);

        /// <summary>Performs one conversion on a channel and returns the hardware count.</summary>
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
