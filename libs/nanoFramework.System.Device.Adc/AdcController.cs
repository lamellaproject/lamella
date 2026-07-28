// nanoFramework.System.Device.Adc (a nanoFramework compatibility assembly) -- System.Device.Adc.AdcController.
using Lamella.Hardware;

namespace System.Device.Adc
{
    /// <summary>Represents an analog-to-digital converter (ADC) controller on the system, in
    /// <b>nanoFramework's</b> shape. .NET has no core ADC class at all, so nothing here corresponds
    /// to a .NET API.</summary>
    /// <remarks>
    /// <para>The namespace is <c>System.Device.Adc</c> because that is nanoFramework's own, which is
    /// what lets nanoFramework source compile here unchanged -- so the namespace alone will not tell
    /// you which framework you are writing against. The assembly's <c>nanoFramework.</c> prefix is
    /// what says so.</para>
    /// <para>The constructor is nanoFramework's parameterless one, and the driver comes from the
    /// board's <see cref="Lamella.Hardware.AdcControllers"/> binding. The one remaining divergence
    /// is in EXTENSION rather than construction: nanoFramework is extended by deriving from its
    /// <c>AdcControllerBase</c>, and there is no <c>AdcControllerBase</c> here -- a platform
    /// supplies an <see cref="Lamella.Hardware.AdcDriver"/> instead. Everything else --
    /// <see cref="ChannelCount"/>, <see cref="ChannelMode"/>, <see cref="MaxValue"/>,
    /// <see cref="MinValue"/>, <see cref="ResolutionInBits"/>, <see cref="IsChannelModeSupported"/>
    /// and <see cref="OpenChannel"/> -- matches member for member.</para>
    /// </remarks>
    public class AdcController
    {
        private readonly AdcDriver _driver;
        private readonly bool[] _open;
        private AdcChannelMode _channelMode;

        /// <summary>Creates a controller over the board's ADC driver.</summary>
        /// <exception cref="System.InvalidOperationException">No ADC driver is bound.</exception>
        public AdcController() : this(Lamella.Hardware.AdcControllers.Resolve())
        {
        }

        internal AdcController(AdcDriver driver)
        {
            _driver = driver;
            _open = new bool[driver.ChannelCount];
            _channelMode = AdcChannelMode.SingleEnded;
        }

        public int ChannelCount { get { return _driver.ChannelCount; } }

        public int ResolutionInBits { get { return _driver.ResolutionInBits; } }

        public int MinValue { get { return _driver.MinValue; } }

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
