// Lamella.Hardware -- the System.Device.Analog controller over the board's bound ADC driver.
using System.Device.Analog;

namespace Lamella.Hardware
{
    internal sealed class AdcAnalogController : AnalogController
    {
        private AdcDriver _driver;

        internal AdcAnalogController()
        {
        }

        private AdcDriver Driver
        {
            get
            {
                if ((object)_driver == null && AdcControllers.IsBound())
                {
                    _driver = AdcControllers.Resolve();
                }
                return _driver;
            }
        }

        public override int PinCount
        {
            get
            {
                AdcDriver driver = Driver;
                if ((object)driver == null)
                {
                    return 0;
                }
                int span = 0;
                int channels = driver.ChannelCount;
                for (int channel = 0; channel < channels; channel++)
                {
                    if (!driver.IsChannelSupported(channel))
                    {
                        continue;
                    }
                    int pin = driver.PinOfChannel(channel);
                    if (pin >= span)
                    {
                        span = pin + 1;
                    }
                }
                return span;
            }
        }

        public override bool SupportsAnalogInput(int pin)
        {
            return ChannelOf(pin) >= 0;
        }

        public override int ConvertPinNumberToAnalogChannelNumber(int pinNumber)
        {
            return ChannelOf(pinNumber);
        }

        public override int ConvertAnalogChannelNumberToPinNumber(int analogChannelNumber)
        {
            AdcDriver driver = Driver;
            if ((object)driver == null || !driver.IsChannelSupported(analogChannelNumber))
            {
                return -1;
            }
            return driver.PinOfChannel(analogChannelNumber);
        }

        protected override AnalogInputPin OpenPinCore(int pinNumber)
        {
            AdcDriver driver = Driver;
            int channel = ChannelOf(pinNumber);
            driver.OpenChannel(channel);
            return new AdcAnalogInputPin(this, pinNumber, channel, driver);
        }

        private int ChannelOf(int pin)
        {
            AdcDriver driver = Driver;
            if ((object)driver == null || pin < 0)
            {
                return -1;
            }
            int channels = driver.ChannelCount;
            for (int channel = 0; channel < channels; channel++)
            {
                if (driver.IsChannelSupported(channel) && driver.PinOfChannel(channel) == pin)
                {
                    return channel;
                }
            }
            return -1;
        }
    }

    internal sealed class AdcAnalogInputPin : AnalogInputPin
    {
        private readonly AdcDriver _driver;
        private readonly int _channel;

        internal AdcAnalogInputPin(AnalogController controller, int pinNumber, int channel, AdcDriver driver)
            : base(controller, pinNumber)
        {
            _driver = driver;
            _channel = channel;
        }

        public override int AdcResolutionBits
        {
            get { return _driver.ResolutionInBits; }
        }

        public override uint ReadRaw()
        {
            int count = _driver.ReadValue(_channel);
            if (count < 0)
            {
                throw new System.IO.IOException("the analog-to-digital conversion failed (status " + (-count) + ")");
            }
            return (uint)count;
        }

        protected override void Dispose(bool disposing)
        {
            if (disposing)
            {
                _driver.CloseChannel(_channel);
            }
            base.Dispose(disposing);
        }
    }
}
