// nanoFramework.System.Device.Adc (a nanoFramework compatibility assembly) -- System.Device.Adc.AdcChannel.
namespace System.Device.Adc
{
    /// <summary>Represents a single ADC channel.</summary>
    public class AdcChannel : System.IDisposable
    {
        private readonly AdcController _controller;
        private readonly int _channelNumber;
        private bool _disposed;

        internal AdcChannel(AdcController controller, int channelNumber)
        {
            _controller = controller;
            _channelNumber = channelNumber;
        }

        public AdcController Controller { get { return _controller; } }

        /// <summary>Reads the digital representation of the analog value from the ADC.</summary>
        public int ReadValue()
        {
            if (_disposed)
            {
                throw new System.InvalidOperationException("channel is disposed");
            }
            return _controller.ReadChannel(_channelNumber);
        }

#if LAMELLA_SURFACE_FLOAT
        /// <summary>Reads the value as a ratio of the max value possible for this controller.</summary>
        public double ReadRatio()
        {
            return ReadValue() / (double)_controller.MaxValue;
        }
#endif

        /// <summary>Releases the connection on this channel, making it available to be opened
        /// by others.</summary>
        public void Dispose()
        {
            if (!_disposed)
            {
                _controller.ReleaseChannel(_channelNumber);
                _disposed = true;
            }
        }
    }
}
