// Lamella System.Device.Gpio -- a separate assembly matching Microsoft's dotnet/iot GPIO API.
namespace System.Device.Gpio
{
    /// <summary>Represents a general-purpose I/O (GPIO) controller.</summary>
    public class GpioController : System.IDisposable
    {
        private readonly GpioDriver _driver;
        private readonly bool[] _open;
        private readonly GpioPin[] _pins;

        /// <summary>Creates a controller over the board's GPIO driver.</summary>
        /// <exception cref="System.PlatformNotSupportedException">The program was built for no
        /// board, or for one with no GPIO controller.</exception>
        public GpioController() : this(BoardDriver())
        {
        }

        private static GpioDriver BoardDriver()
        {
            GpioDriver driver = Lamella.Hardware.Buses.FindGpio();
            if ((object)driver == null)
            {
                throw new System.PlatformNotSupportedException(Lamella.Hardware.Buses.MissingGpio());
            }
            return driver;
        }

        /// <summary>Creates a controller over <paramref name="driver"/>.</summary>
        /// <param name="driver">The driver that manages all of the pin operations for the
        /// controller.</param>
        public GpioController(GpioDriver driver)
        {
            _driver = driver;
            if ((object)driver != null)
            {
                _open = new bool[driver.PinCount];
                _pins = new GpioPin[driver.PinCount];
            }
        }

        /// <summary>The number of pins provided by the controller.</summary>
        public virtual int PinCount
        {
            get
            {
                CheckDriverValid();
                return _driver.PinCount;
            }
        }

        private void CheckDriverValid()
        {
            if ((object)_driver == null)
            {
                throw new System.ObjectDisposedException("GpioController");
            }
        }

        /// <summary>Opens a pin so it is ready to use, and returns it; a pin that is open already is
        /// returned as it is.</summary>
        public GpioPin OpenPin(int pinNumber)
        {
            if (!IsPinOpen(pinNumber))
            {
                OpenPinCore(pinNumber);
            }
            GpioPin pin = _pins[pinNumber];
            if ((object)pin == null)
            {
                pin = new GpioPin(pinNumber, this);
                _pins[pinNumber] = pin;
            }
            return pin;
        }

        /// <summary>Opens a pin and sets it to a specific mode.</summary>
        public GpioPin OpenPin(int pinNumber, PinMode mode)
        {
            GpioPin pin = OpenPin(pinNumber);
            SetPinMode(pinNumber, mode);
            return pin;
        }

        /// <summary>Opens a pin, sets its mode, and drives an initial value.</summary>
        public GpioPin OpenPin(int pinNumber, PinMode mode, PinValue initialValue)
        {
            GpioPin pin = OpenPin(pinNumber, mode);
            Write(pinNumber, initialValue);
            return pin;
        }

        /// <summary>Whether a specific pin is open.</summary>
        public virtual bool IsPinOpen(int pinNumber)
        {
            CheckDriverValid();
            return pinNumber >= 0 && pinNumber < _open.Length && _open[pinNumber];
        }

        /// <summary>Closes an open pin.</summary>
        public void ClosePin(int pinNumber)
        {
            ClosePinCore(pinNumber);
        }

        /// <summary>Opens a pin. The overridable half of <see cref="OpenPin(int)"/>.</summary>
        /// <param name="pinNumber">The pin number, in this controller's numbering scheme.</param>
        protected virtual void OpenPinCore(int pinNumber)
        {
            CheckDriverValid();
            _driver.OpenPin(pinNumber);
            _open[pinNumber] = true;
        }

        /// <summary>Closes a pin. The overridable half of <see cref="ClosePin(int)"/>.</summary>
        /// <param name="pinNumber">The pin number, in this controller's numbering scheme.</param>
        protected virtual void ClosePinCore(int pinNumber)
        {
            CheckDriverValid();
            _driver.ClosePin(pinNumber);
            _open[pinNumber] = false;
            _pins[pinNumber] = null;
        }

        /// <summary>Sets the mode of a pin.</summary>
        public virtual void SetPinMode(int pinNumber, PinMode mode)
        {
            CheckDriverValid();
            _driver.SetPinMode(pinNumber, mode);
        }

        /// <summary>Gets the mode of a pin.</summary>
        public virtual PinMode GetPinMode(int pinNumber)
        {
            CheckDriverValid();
            return _driver.GetPinMode(pinNumber);
        }

        /// <summary>Whether a pin supports a specific mode.</summary>
        public virtual bool IsPinModeSupported(int pinNumber, PinMode mode)
        {
            CheckDriverValid();
            return _driver.IsPinModeSupported(pinNumber, mode);
        }

        /// <summary>Reads the current value of a pin.</summary>
        public virtual PinValue Read(int pinNumber)
        {
            CheckDriverValid();
            return _driver.Read(pinNumber);
        }

        /// <summary>Writes a value to a pin.</summary>
        public virtual void Write(int pinNumber, PinValue value)
        {
            CheckDriverValid();
            _driver.Write(pinNumber, value);
        }

        /// <summary>Toggles the current value of a pin.</summary>
        public virtual void Toggle(int pinNumber)
        {
            CheckDriverValid();
            _driver.Toggle(pinNumber);
        }

        /// <summary>Adds a callback that is invoked when <paramref name="pinNumber"/> has an event
        /// of type <paramref name="eventTypes"/>.</summary>
        public virtual void RegisterCallbackForPinValueChangedEvent(
            int pinNumber, PinEventTypes eventTypes, PinChangeEventHandler callback)
        {
            CheckDriverValid();
            _driver.AddCallbackForPinValueChangedEvent(pinNumber, eventTypes, callback);
        }

        /// <summary>Removes a callback that was being invoked for the pin at
        /// <paramref name="pinNumber"/>.</summary>
        public virtual void UnregisterCallbackForPinValueChangedEvent(int pinNumber, PinChangeEventHandler callback)
        {
            CheckDriverValid();
            _driver.RemoveCallbackForPinValueChangedEvent(pinNumber, callback);
        }

        /// <summary>Disposes the controller and its driver.</summary>
        public void Dispose()
        {
            Dispose(true);
        }

        /// <summary>Releases the resources this controller holds.</summary>
        /// <param name="disposing">
        /// True when called from <see cref="Dispose()"/>; false when called from a finalizer.
        /// </param>
        protected virtual void Dispose(bool disposing)
        {
            if (disposing && (object)_driver != null)
            {
                _driver.Dispose();
            }
        }

        /// <summary>Blocks until an edge of one of <paramref name="eventTypes"/> is seen on a pin, or the timeout elapses.</summary>
        /// <param name="pinNumber">The pin number, in this controller's numbering scheme.</param>
        /// <param name="eventTypes">The edges to wait for.</param>
        /// <param name="timeout">How long to wait before giving up.</param>
        /// <returns>Which edge was seen, or a result whose TimedOut is true.</returns>
        public WaitForEventResult WaitForEvent(int pinNumber, PinEventTypes eventTypes, System.TimeSpan timeout)
        {
            WaitForEventResult result = new WaitForEventResult();
            PinValue start = Read(pinNumber);
            long deadline = System.DateTime.UtcNow.Ticks + timeout.Ticks;
            while (System.DateTime.UtcNow.Ticks < deadline)
            {
                PinValue now = Read(pinNumber);
                if (now != start)
                {
                    PinEventTypes edge = (now == PinValue.High) ? PinEventTypes.Rising : PinEventTypes.Falling;
                    if ((eventTypes & edge) != PinEventTypes.None)
                    {
                        result.EventTypes = edge;
                        result.TimedOut = false;
                        return result;
                    }
                    start = now;
                }
            }
            result.EventTypes = PinEventTypes.None;
            result.TimedOut = true;
            return result;
        }
    }
}
