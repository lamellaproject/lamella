// System.Device.Adc -- third-party-compatibility ADC surface shaped after nanoFramework's Windows.Devices.Adc (dotnet/iot ships no core ADC class), shipped as its own assembly.
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

        public void Dispose()
        {
            Close();
        }
    }
}
