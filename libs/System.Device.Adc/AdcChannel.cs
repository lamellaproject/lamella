// Lamella System.Device.Adc -- a separate assembly matching the Windows.Devices.Adc /
namespace System.Device.Adc
{
    /// <summary>Represents a single ADC channel.</summary>
    public class AdcChannel : System.IDisposable
    {
        private readonly AdcController _controller;
        private readonly int _channelNumber;
        private bool _closed;

        internal AdcChannel(AdcController controller, int channelNumber)
        {
            _controller = controller;
            _channelNumber = channelNumber;
        }

        /// <summary>Gets the ADC controller for this channel.</summary>
        public AdcController Controller { get { return _controller; } }

        /// <summary>Reads the digital representation of the analog value from the ADC.</summary>
        public int ReadValue()
        {
            if (_closed)
            {
                throw new System.InvalidOperationException("channel is closed");
            }
            return _controller.ReadChannel(_channelNumber);
        }

        /// <summary>Reads the value as a ratio of the max value possible for this controller.</summary>
        public double ReadRatio()
        {
            return ReadValue() / (double)_controller.MaxValue;
        }

        /// <summary>Closes the connection on this channel, making it available to be opened
        /// by others.</summary>
        public void Close()
        {
            if (!_closed)
            {
                _controller.ReleaseChannel(_channelNumber);
                _closed = true;
            }
        }

        /// <summary>Disposes the channel, closing it.</summary>
        public void Dispose()
        {
            Close();
        }
    }
}
