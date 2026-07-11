// Lamella System.Device.Adc -- a separate assembly matching the Windows.Devices.Adc /
namespace System.Device.Adc
{
    /// <summary>Represents an analog-to-digital converter (ADC) controller on the system.</summary>
    public class AdcController
    {
        private readonly AdcDriver _driver;
        private readonly bool[] _open;
        private AdcChannelMode _channelMode;

        /// <summary>Creates a controller over <paramref name="driver"/>.</summary>
        public AdcController(AdcDriver driver)
        {
            _driver = driver;
            _open = new bool[driver.ChannelCount];
            _channelMode = AdcChannelMode.SingleEnded;
        }

        /// <summary>The number of channels available on the ADC controller.</summary>
        public int ChannelCount { get { return _driver.ChannelCount; } }

        /// <summary>The resolution of the controller as the number of bits it has.</summary>
        public int ResolutionInBits { get { return _driver.ResolutionInBits; } }

        /// <summary>The minimum value the controller can report.</summary>
        public int MinValue { get { return _driver.MinValue; } }

        /// <summary>The maximum value that the controller can report.</summary>
        public int MaxValue { get { return _driver.MaxValue; } }

        /// <summary>The channel mode for the ADC controller. Setting a mode the hardware does
        /// not support throws <see cref="System.ArgumentException"/>.</summary>
        public AdcChannelMode ChannelMode
        {
            get { return _channelMode; }
            set
            {
                if (!_driver.IsChannelModeSupported(value))
                {
                    throw new System.ArgumentException("channel mode not supported");
                }
                _driver.SetChannelMode(value);
                _channelMode = value;
            }
        }

        /// <summary>Verifies that the specified channel mode is supported by the controller.</summary>
        public bool IsChannelModeSupported(AdcChannelMode channelMode)
        {
            return _driver.IsChannelModeSupported(channelMode);
        }

        /// <summary>Opens a connection to the specified ADC channel. A channel is exclusive:
        /// opening one that is already open throws <see cref="System.InvalidOperationException"/>
        /// until its holder closes it.</summary>
        public AdcChannel OpenChannel(int channelNumber)
        {
            if (channelNumber < 0 || channelNumber >= _driver.ChannelCount)
            {
                throw new System.ArgumentOutOfRangeException("channelNumber");
            }
            if (_open[channelNumber])
            {
                throw new System.InvalidOperationException("channel in use");
            }
            _driver.OpenChannel(channelNumber);
            _open[channelNumber] = true;
            return new AdcChannel(this, channelNumber);
        }

        internal int ReadChannel(int channelNumber)
        {
            return _driver.ReadValue(channelNumber);
        }

        internal void ReleaseChannel(int channelNumber)
        {
            _driver.CloseChannel(channelNumber);
            _open[channelNumber] = false;
        }
    }
}
