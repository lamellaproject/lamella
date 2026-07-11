// Lamella System.Device.Adc -- a separate assembly matching the Windows.Devices.Adc /
namespace System.Device.Adc
{
    /// <summary>Base class for ADC drivers: convert analog inputs to digital counts.</summary>
    public abstract class AdcDriver : System.IDisposable
    {
        /// <summary>The number of channels the converter provides.</summary>
        protected internal abstract int ChannelCount { get; }

        /// <summary>The converter's resolution in bits.</summary>
        protected internal abstract int ResolutionInBits { get; }

        /// <summary>The minimum count the converter can report.</summary>
        protected internal abstract int MinValue { get; }

        /// <summary>The maximum count the converter can report.</summary>
        protected internal abstract int MaxValue { get; }

        /// <summary>Whether the converter supports a specific channel mode.</summary>
        protected internal abstract bool IsChannelModeSupported(AdcChannelMode mode);

        /// <summary>Applies a channel mode to the converter.</summary>
        protected internal abstract void SetChannelMode(AdcChannelMode mode);

        /// <summary>Claims a channel and runs its per-channel enable steps (pad-to-analog
        /// prep for pin-backed channels, bias enables for internal sources).</summary>
        protected internal abstract void OpenChannel(int channel);

        /// <summary>Releases a claimed channel.</summary>
        protected internal abstract void CloseChannel(int channel);

        /// <summary>Performs one conversion on a channel and returns the hardware count.</summary>
        protected internal abstract int ReadValue(int channel);

        /// <summary>Disposes this instance.</summary>
        public void Dispose()
        {
            Dispose(true);
        }

        /// <summary>Releases the driver's resources.</summary>
        protected virtual void Dispose(bool disposing)
        {
        }
    }
}
