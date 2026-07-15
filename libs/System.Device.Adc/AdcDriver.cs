// System.Device.Adc -- third-party-compatibility ADC surface shaped after nanoFramework's Windows.Devices.Adc (dotnet/iot ships no core ADC class), shipped as its own assembly.
namespace System.Device.Adc
{
    /// <summary>Base class for ADC drivers: convert analog inputs to digital counts.</summary>
    public abstract class AdcDriver : System.IDisposable
    {
        protected internal abstract int ChannelCount { get; }

        protected internal abstract int ResolutionInBits { get; }

        protected internal abstract int MinValue { get; }

        protected internal abstract int MaxValue { get; }

        protected internal abstract bool IsChannelModeSupported(AdcChannelMode mode);

        protected internal abstract void SetChannelMode(AdcChannelMode mode);

        /// <summary>Claims a channel and runs its per-channel enable steps (pad-to-analog
        /// prep for pin-backed channels, bias enables for internal sources).</summary>
        protected internal abstract void OpenChannel(int channel);

        protected internal abstract void CloseChannel(int channel);

        /// <summary>Performs one conversion on a channel and returns the hardware count.</summary>
        protected internal abstract int ReadValue(int channel);

        public void Dispose()
        {
            Dispose(true);
        }

        protected virtual void Dispose(bool disposing)
        {
        }
    }
}
