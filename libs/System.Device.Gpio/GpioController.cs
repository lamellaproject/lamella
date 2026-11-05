// Lamella System.Device.Gpio -- a separate assembly matching Microsoft's dotnet/iot GPIO API.
namespace System.Device.Gpio
{
    /// <summary>Represents a general-purpose I/O (GPIO) controller.</summary>
    public class GpioController : System.IDisposable
    {
        private GpioDriver _driver;
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

        private void CheckOpen(int pinNumber, string refusal)
        {
            if (!IsPinOpen(pinNumber))
            {
                throw new System.InvalidOperationException(refusal + pinNumber.ToString() + " because it is not open.");
            }
        }

        /// <summary>Opens a pin so it is ready to use, and returns it; a pin that is open already is
        /// returned as it is.</summary>
        public GpioPin OpenPin(int pinNumber)
        {
            if (!IsPinOpen(pinNumber))
            {
                OpenPinCore(pinNumber);
                if (pinNumber < 0 || pinNumber >= _open.Length)
                {
                    throw new System.ArgumentOutOfRangeException("pinNumber", "This controller has no such pin.");
                }
                _open[pinNumber] = true;
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
        /// <exception cref="System.InvalidOperationException">The pin is not open.</exception>
        public void ClosePin(int pinNumber)
        {
            CheckOpen(pinNumber, "Can not close pin ");
            ClosePinCore(pinNumber);
            _open[pinNumber] = false;
            _pins[pinNumber] = null;
        }

        /// <summary>Opens a pin. The overridable half of <see cref="OpenPin(int)"/>.</summary>
        /// <param name="pinNumber">The pin number, in this controller's numbering scheme.</param>
        /// <exception cref="System.ArgumentOutOfRangeException">The controller has no such pin.</exception>
        protected virtual void OpenPinCore(int pinNumber)
        {
            CheckDriverValid();
            _driver.OpenPin(pinNumber);
        }

        /// <summary>Closes a pin. The overridable half of <see cref="ClosePin(int)"/>.</summary>
        /// <param name="pinNumber">The pin number, in this controller's numbering scheme.</param>
        protected virtual void ClosePinCore(int pinNumber)
        {
            CheckDriverValid();
            _driver.ClosePin(pinNumber);
        }

        /// <summary>Sets the mode of a pin.</summary>
        /// <exception cref="System.InvalidOperationException">The pin is not open.</exception>
        public virtual void SetPinMode(int pinNumber, PinMode mode)
        {
            CheckOpen(pinNumber, "Can not set a mode to pin ");
            _driver.SetPinMode(pinNumber, mode);
        }

        /// <summary>Gets the mode of a pin.</summary>
        /// <exception cref="System.InvalidOperationException">The pin is not open.</exception>
        public virtual PinMode GetPinMode(int pinNumber)
        {
            CheckOpen(pinNumber, "Can not get the mode of pin ");
            return _driver.GetPinMode(pinNumber);
        }

        /// <summary>Whether a pin supports a specific mode.</summary>
        public virtual bool IsPinModeSupported(int pinNumber, PinMode mode)
        {
            CheckDriverValid();
            return _driver.IsPinModeSupported(pinNumber, mode);
        }

        /// <summary>Reads the current value of a pin.</summary>
        /// <exception cref="System.InvalidOperationException">The pin is not open.</exception>
        public virtual PinValue Read(int pinNumber)
        {
            CheckOpen(pinNumber, "Can not read from pin ");
            return _driver.Read(pinNumber);
        }

        /// <summary>Writes a value to a pin.</summary>
        /// <exception cref="System.InvalidOperationException">The pin is not open.</exception>
        public virtual void Write(int pinNumber, PinValue value)
        {
            CheckOpen(pinNumber, "Can not write to pin ");
            _driver.Write(pinNumber, value);
        }

        /// <summary>Toggles the current value of a pin.</summary>
        /// <exception cref="System.InvalidOperationException">The pin is not open.</exception>
        public virtual void Toggle(int pinNumber)
        {
            CheckOpen(pinNumber, "Can not read from pin ");
            _driver.Toggle(pinNumber);
        }

        /// <summary>Adds a callback that is invoked when <paramref name="pinNumber"/> has an event
        /// of type <paramref name="eventTypes"/>.</summary>
        /// <exception cref="System.InvalidOperationException">The pin is not open.</exception>
        public virtual void RegisterCallbackForPinValueChangedEvent(
            int pinNumber, PinEventTypes eventTypes, PinChangeEventHandler callback)
        {
            CheckOpen(pinNumber, "Can not add callback for pin ");
            _driver.AddCallbackForPinValueChangedEvent(pinNumber, eventTypes, callback);
        }

        /// <summary>Removes a callback that was being invoked for the pin at
        /// <paramref name="pinNumber"/>.</summary>
        /// <exception cref="System.InvalidOperationException">The pin is not open.</exception>
        public virtual void UnregisterCallbackForPinValueChangedEvent(int pinNumber, PinChangeEventHandler callback)
        {
            CheckOpen(pinNumber, "Can not remove callback for pin ");
            _driver.RemoveCallbackForPinValueChangedEvent(pinNumber, callback);
        }

        /// <summary>Closes every pin this controller opened and disposes its driver; every member but this
        /// one then throws <see cref="System.ObjectDisposedException"/>.</summary>
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
                for (int pin = 0; pin < _open.Length; pin = pin + 1)
                {
                    if (_open[pin])
                    {
                        ClosePinCore(pin);
                        _open[pin] = false;
                        _pins[pin] = null;
                    }
                }
                _driver.Dispose();
                _driver = null;
            }
        }

        /// <summary>Blocks until an edge of one of <paramref name="eventTypes"/> is seen on a pin, or the timeout elapses.</summary>
        /// <param name="pinNumber">The pin number, in this controller's numbering scheme.</param>
        /// <param name="eventTypes">The edges to wait for.</param>
        /// <param name="timeout">How long to wait before giving up.</param>
        /// <returns>Which edge was seen, or a result whose TimedOut is true.</returns>
        /// <exception cref="System.InvalidOperationException">The pin is not open.</exception>
        public WaitForEventResult WaitForEvent(int pinNumber, PinEventTypes eventTypes, System.TimeSpan timeout)
        {
            CheckOpen(pinNumber, "Can not wait for events from pin ");
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
