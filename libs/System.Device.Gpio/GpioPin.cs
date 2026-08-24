// Lamella System.Device.Gpio -- a separate assembly matching Microsoft's dotnet/iot GPIO API.
namespace System.Device.Gpio
{
    /// <summary>Represents a general-purpose I/O (GPIO) pin.</summary>
    public class GpioPin
    {
        private readonly GpioController _controller;
        private readonly int _pinNumber;

        internal GpioPin(GpioController controller, int pinNumber)
        {
            _controller = controller;
            _pinNumber = pinNumber;
        }

        /// <summary>The pin number of this pin.</summary>
        public virtual int PinNumber { get { return _pinNumber; } }

        /// <summary>Gets the current mode of this pin.</summary>
        public virtual PinMode GetPinMode() { return _controller.GetPinMode(_pinNumber); }

        /// <summary>Whether this pin supports a specific mode.</summary>
        public virtual bool IsPinModeSupported(PinMode pinMode) { return _controller.IsPinModeSupported(_pinNumber, pinMode); }

        /// <summary>Sets the mode of this pin.</summary>
        public virtual void SetPinMode(PinMode value) { _controller.SetPinMode(_pinNumber, value); }

        /// <summary>Reads the current value of this pin.</summary>
        public virtual PinValue Read() { return _controller.Read(_pinNumber); }

        /// <summary>Drives a value onto this pin (when configured as an output).</summary>
        public virtual void Write(PinValue value) { _controller.Write(_pinNumber, value); }

        /// <summary>Toggles the output of this pin (when configured as an output).</summary>
        public virtual void Toggle() { _controller.Toggle(_pinNumber); }

        /// <summary>Occurs when the value of this pin changes.</summary>
        public virtual event PinChangeEventHandler ValueChanged
        {
            add
            {
                _controller.RegisterCallbackForPinValueChangedEvent(
                    _pinNumber, PinEventTypes.Rising | PinEventTypes.Falling, value);
            }
            remove
            {
                _controller.UnregisterCallbackForPinValueChangedEvent(_pinNumber, value);
            }
        }
    }
}
