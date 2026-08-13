// Lamella System.Device.Gpio -- a separate assembly matching Microsoft's dotnet/iot GPIO API.
namespace System.Device.Gpio
{
    /// <summary>Arguments passed in when an event is triggered by the GPIO.</summary>
    public class PinValueChangedEventArgs : System.EventArgs
    {
        private readonly PinEventTypes _changeType;
        private readonly int _pinNumber;

        /// <summary>Creates the arguments for a pin value changed event.</summary>
        /// <param name="changeType">The change type that triggered the event.</param>
        /// <param name="pinNumber">The pin number that triggered the event.</param>
        public PinValueChangedEventArgs(PinEventTypes changeType, int pinNumber)
        {
            _changeType = changeType;
            _pinNumber = pinNumber;
        }

        /// <summary>The change type that triggered the event.</summary>
        public PinEventTypes ChangeType { get { return _changeType; } }

        /// <summary>The pin number that triggered the event.</summary>
        public int PinNumber { get { return _pinNumber; } }
    }
}
