// Lamella System.Device.Gpio -- a separate assembly matching Microsoft's dotnet/iot GPIO API.
namespace System.Device.Gpio
{
    /// <summary>Base class for GPIO drivers: read from and write to digital I/O pins.</summary>
    public abstract class GpioDriver : System.IDisposable
    {
        /// <summary>The number of pins provided by the driver.</summary>
        protected internal abstract int PinCount { get; }

        /// <summary>Converts a board pin number to the driver's logical numbering scheme.</summary>
        protected internal abstract int ConvertPinNumberToLogicalNumberingScheme(int pinNumber);

        /// <summary>Opens a pin so it is ready to use, without changing its mode or value.</summary>
        protected internal abstract void OpenPin(int pinNumber);

        /// <summary>Closes an open pin.</summary>
        protected internal abstract void ClosePin(int pinNumber);

        /// <summary>Sets the mode of a pin (input/output/pull-up/pull-down).</summary>
        protected internal abstract void SetPinMode(int pinNumber, PinMode mode);

        /// <summary>Gets the mode of a pin.</summary>
        protected internal abstract PinMode GetPinMode(int pinNumber);

        /// <summary>Whether a pin supports a specific mode.</summary>
        protected internal abstract bool IsPinModeSupported(int pinNumber, PinMode mode);

        /// <summary>Reads the current value of a pin.</summary>
        protected internal abstract PinValue Read(int pinNumber);

        /// <summary>Writes a value to a pin.</summary>
        protected internal abstract void Write(int pinNumber, PinValue value);

        /// <summary>Toggles the current value of a pin. The default reads then writes the inverse;
        /// a driver may override with a hardware toggle.</summary>
        protected internal virtual void Toggle(int pinNumber)
        {
            Write(pinNumber, !Read(pinNumber));
        }

        /// <summary>Adds a handler that is invoked when <paramref name="pinNumber"/> sees an event
        /// of type <paramref name="eventTypes"/>.</summary>
        protected internal abstract void AddCallbackForPinValueChangedEvent(
            int pinNumber, PinEventTypes eventTypes, PinChangeEventHandler callback);

        /// <summary>Removes a handler that was being invoked for the pin at
        /// <paramref name="pinNumber"/>.</summary>
        protected internal abstract void RemoveCallbackForPinValueChangedEvent(
            int pinNumber, PinChangeEventHandler callback);

        /// <summary>Disposes this instance, closing all open pins.</summary>
        public void Dispose()
        {
            Dispose(true);
        }

        /// <summary>Releases the driver's resources.</summary>
        protected virtual void Dispose(bool disposing)
        {
        }
    }
}
