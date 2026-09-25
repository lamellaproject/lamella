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
                if ((object)_driver == null)
                {
                    _driver = AdcControllers.Resolve();
                }
                return _driver;
            }
        }

        public override int PinCount
        {
            get { return Driver.ChannelCount; }
        }

        public override bool SupportsAnalogInput(int pin)
        {
            return Driver.IsChannelSupported(pin);
        }

        protected override AnalogInputPin OpenPinCore(int pinNumber)
        {
            AdcDriver driver = Driver;
            driver.OpenChannel(pinNumber);
            return new AdcAnalogInputPin(this, pinNumber, driver);
        }
    }

    internal sealed class AdcAnalogInputPin : AnalogInputPin
    {
        private readonly AdcDriver _driver;

        internal AdcAnalogInputPin(AnalogController controller, int pinNumber, AdcDriver driver)
            : base(controller, pinNumber)
        {
            _driver = driver;
        }

        public override int AdcResolutionBits
        {
            get { return _driver.ResolutionInBits; }
        }

        public override uint ReadRaw()
        {
            int count = _driver.ReadValue(PinNumber);
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
                _driver.CloseChannel(PinNumber);
            }
            base.Dispose(disposing);
        }
    }
}
