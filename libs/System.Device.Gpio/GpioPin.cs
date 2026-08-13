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
        public int PinNumber { get { return _pinNumber; } }

        /// <summary>Gets the current mode of this pin.</summary>
        public PinMode GetPinMode() { return _controller.GetPinMode(_pinNumber); }

        /// <summary>Whether this pin supports a specific mode.</summary>
        public bool IsPinModeSupported(PinMode mode) { return _controller.IsPinModeSupported(_pinNumber, mode); }

        /// <summary>Sets the mode of this pin.</summary>
        public void SetPinMode(PinMode mode) { _controller.SetPinMode(_pinNumber, mode); }

        /// <summary>Reads the current value of this pin.</summary>
        public PinValue Read() { return _controller.Read(_pinNumber); }

        /// <summary>Drives a value onto this pin (when configured as an output).</summary>
        public void Write(PinValue value) { _controller.Write(_pinNumber, value); }

        /// <summary>Toggles the output of this pin (when configured as an output).</summary>
        public void Toggle() { _controller.Toggle(_pinNumber); }

        /// <summary>Occurs when the value of this pin changes.</summary>
        public event PinChangeEventHandler ValueChanged
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
