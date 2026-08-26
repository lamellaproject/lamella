// Lamella System.Device.Gpio -- a separate assembly matching Microsoft's dotnet/iot GPIO API.
namespace System.Device.Gpio
{
    /// <summary>Base class for GPIO drivers: read from and write to digital I/O pins.</summary>
    public abstract class GpioDriver : System.IDisposable
    {
        /// <summary>Releases the driver's resources if it was never disposed explicitly.</summary>
        ~GpioDriver()
        {
            Dispose(false);
        }

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

        /// <summary>Sets a pin's mode and drives an initial value.</summary>
        /// <param name="pinNumber">The pin number, in the driver's logical scheme.</param>
        /// <param name="mode">The mode to set.</param>
        /// <param name="initialValue">The value to drive once the mode is set.</param>
        protected internal virtual void SetPinMode(int pinNumber, PinMode mode, PinValue initialValue)
        {
            SetPinMode(pinNumber, mode);
            Write(pinNumber, initialValue);
        }

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
            System.GC.SuppressFinalize(this);
        }

        /// <summary>Releases the driver's resources.</summary>
        protected virtual void Dispose(bool disposing)
        {
        }
    }
}
