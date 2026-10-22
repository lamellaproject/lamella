// Lamella System.Device.Gpio -- a separate assembly matching Microsoft's dotnet/iot GPIO API.
namespace System.Device.Gpio
{
    /// <summary>Represents a general-purpose I/O (GPIO) pin.</summary>
    public class GpioPin :
#if LAMELLA_SURFACE_NETFX_2_0
        System.IEquatable<GpioPin>,
#endif
        System.IDisposable
    {
        private readonly int _pinNumber;
        private readonly GpioController _controller;

        /// <summary>Creates a pin over <paramref name="controller"/>.</summary>
        /// <param name="pinNumber">The pin number, in the controller's numbering scheme.</param>
        /// <param name="controller">The controller the pin belongs to.</param>
        /// <remarks>A program usually gets its pins from <see cref="GpioController.OpenPin(int)"/>
        /// rather than constructing them.</remarks>
        protected internal GpioPin(int pinNumber, GpioController controller)
        {
            _controller = controller;
            _pinNumber = pinNumber;
        }

        /// <summary>The pin number of this pin.</summary>
        public virtual int PinNumber { get { return _pinNumber; } }

        /// <summary>The controller this pin belongs to.</summary>
        public GpioController Controller { get { return _controller; } }

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

        /// <summary>Whether <paramref name="other"/> is the same pin of the same controller.</summary>
        /// <param name="other">The pin to compare with, which may be null.</param>
        public virtual bool Equals(GpioPin other)
        {
            if ((object)other == null)
            {
                return false;
            }
            return PinNumber == other.PinNumber && (object)_controller == (object)other._controller;
        }

        /// <summary>Whether <paramref name="obj"/> is the same pin of the same controller.</summary>
        /// <param name="obj">The object to compare with, which may be null.</param>
        public override bool Equals(object obj)
        {
            if (obj == null)
            {
                return false;
            }
            if ((object)this == obj)
            {
                return true;
            }
            GpioPin other = obj as GpioPin;
            if ((object)other != null)
            {
                return Equals(other);
            }
            return false;
        }

        /// <summary>A hash code combining the controller's and the pin number.</summary>
        public override int GetHashCode()
        {
            return _controller.GetHashCode() ^ _pinNumber;
        }

        /// <summary>Closes the pin, if it is open.</summary>
        public virtual void Close()
        {
            if (_controller.IsPinOpen(_pinNumber))
            {
                _controller.ClosePin(_pinNumber);
            }
        }

        /// <summary>Closes the pin. The overridable half of <see cref="Dispose()"/>.</summary>
        /// <param name="disposing">True when called from <see cref="Dispose()"/>.</param>
        protected virtual void Dispose(bool disposing)
        {
            if (disposing)
            {
                Close();
            }
        }

        /// <summary>Closes the pin.</summary>
        public void Dispose()
        {
            Dispose(true);
            System.GC.SuppressFinalize(this);
        }
    }
}
