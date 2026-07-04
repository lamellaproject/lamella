// Lamella System.Device.Gpio -- a separate assembly matching Microsoft's dotnet/iot GPIO API.
namespace System.Device.Gpio
{
    /// <summary>Represents a general-purpose I/O (GPIO) controller.</summary>
    public class GpioController : System.IDisposable
    {
        private readonly GpioDriver _driver;
        private readonly PinNumberingScheme _numbering;
        private readonly bool[] _open;

        /// <summary>Creates a controller over <paramref name="driver"/> using the given numbering scheme.</summary>
        public GpioController(PinNumberingScheme numberingScheme, GpioDriver driver)
        {
            _numbering = numberingScheme;
            _driver = driver;
            _open = new bool[driver.PinCount];
        }

        /// <summary>Creates a controller over <paramref name="driver"/> using logical pin numbering.</summary>
        public GpioController(GpioDriver driver) : this(PinNumberingScheme.Logical, driver)
        {
        }

        /// <summary>The numbering scheme used to represent pins.</summary>
        public PinNumberingScheme NumberingScheme { get { return _numbering; } }

        /// <summary>The number of pins provided by the controller.</summary>
        public int PinCount { get { return _driver.PinCount; } }

        private int Logical(int pinNumber)
        {
            if (_numbering == PinNumberingScheme.Logical)
            {
                return pinNumber;
            }
            return _driver.ConvertPinNumberToLogicalNumberingScheme(pinNumber);
        }

        /// <summary>Opens a pin so it is ready to use.</summary>
        public GpioPin OpenPin(int pinNumber)
        {
            int logical = Logical(pinNumber);
            _driver.OpenPin(logical);
            _open[logical] = true;
            return new GpioPin(this, pinNumber);
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
        public bool IsPinOpen(int pinNumber) { return _open[Logical(pinNumber)]; }

        /// <summary>Closes an open pin.</summary>
        public void ClosePin(int pinNumber)
        {
            int logical = Logical(pinNumber);
            _driver.ClosePin(logical);
            _open[logical] = false;
        }

        /// <summary>Sets the mode of a pin.</summary>
        public void SetPinMode(int pinNumber, PinMode mode) { _driver.SetPinMode(Logical(pinNumber), mode); }

        /// <summary>Gets the mode of a pin.</summary>
        public PinMode GetPinMode(int pinNumber) { return _driver.GetPinMode(Logical(pinNumber)); }

        /// <summary>Whether a pin supports a specific mode.</summary>
        public bool IsPinModeSupported(int pinNumber, PinMode mode) { return _driver.IsPinModeSupported(Logical(pinNumber), mode); }

        /// <summary>Reads the current value of a pin.</summary>
        public PinValue Read(int pinNumber) { return _driver.Read(Logical(pinNumber)); }

        /// <summary>Writes a value to a pin.</summary>
        public void Write(int pinNumber, PinValue value) { _driver.Write(Logical(pinNumber), value); }

        /// <summary>Toggles the current value of a pin.</summary>
        public void Toggle(int pinNumber) { _driver.Toggle(Logical(pinNumber)); }

        /// <summary>Disposes the controller and its driver.</summary>
        public void Dispose() { _driver.Dispose(); }
    }
}
