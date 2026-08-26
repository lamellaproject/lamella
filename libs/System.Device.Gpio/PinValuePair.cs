// Lamella System.Device.Gpio -- a separate assembly matching Microsoft's dotnet/iot GPIO API.
namespace System.Device.Gpio
{
    /// <summary>A pin number paired with the value to write to it, for a multi-pin write.</summary>
    public struct PinValuePair
    {
        private readonly int _pinNumber;
        private readonly PinValue _pinValue;

        /// <summary>Creates a pair of a pin number and a pin value.</summary>
        /// <param name="pinNumber">The pin number, in the controller's numbering scheme.</param>
        /// <param name="pinValue">The value that belongs with it.</param>
        public PinValuePair(int pinNumber, PinValue pinValue)
        {
            _pinNumber = pinNumber;
            _pinValue = pinValue;
        }

        /// <summary>The pin number.</summary>
        public int PinNumber { get { return _pinNumber; } }

        /// <summary>The pin value.</summary>
        public PinValue PinValue { get { return _pinValue; } }

        /// <summary>Splits this pair into its pin number and its value.</summary>
        /// <param name="pinNumber">Receives the pin number.</param>
        /// <param name="pinValue">Receives the pin value.</param>
        public void Deconstruct(out int pinNumber, out PinValue pinValue)
        {
            pinNumber = _pinNumber;
            pinValue = _pinValue;
        }
    }
}
