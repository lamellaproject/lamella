// System.Device.Analog -- the dotnet/iot ANALOG INPUT surface: abstract AnalogController : IDisposable
namespace System.Device.Analog
{
    /// <summary>Base class for analog controllers.</summary>
    /// <remarks>
    /// <para>A platform derives from this and supplies <see cref="OpenPinCore"/>, exactly as in
    /// dotnet/iot. The controller owns which pins are open (opening one twice is an error) and the
    /// pins themselves report values.</para>
    /// <para><b>The voltage-bearing half of the upstream surface is not here yet</b> -- see the list on
    /// <see cref="AnalogInputPin"/>. What ships is the raw-count surface, which is complete and needs
    /// no unit type.</para>
    /// </remarks>
    public abstract class AnalogController : System.IDisposable
    {
        private AnalogInputPin[] _openPins;
        private int _openCount;

        /// <summary>Constructs an instance of an analog controller.</summary>
        protected AnalogController()
        {
        }

        /// <summary>The number of pins this controller has.</summary>
        public abstract int PinCount { get; }

        /// <summary>Whether the given pin supports analog input.</summary>
        public abstract bool SupportsAnalogInput(int pin);

        /// <summary>Converts a logical pin number to the analog channel number behind it. The default
        /// is the identity, which is what a controller whose pins ARE its channels wants.</summary>
        public virtual int ConvertPinNumberToAnalogChannelNumber(int pinNumber)
        {
            return pinNumber;
        }

        /// <summary>Converts an analog channel number to the logical pin number in front of it. The
        /// default is the identity.</summary>
        public virtual int ConvertAnalogChannelNumberToPinNumber(int analogChannelNumber)
        {
            return analogChannelNumber;
        }

        /// <summary>Opens an analog input pin.</summary>
        /// <exception cref="System.NotSupportedException">The pin does not support analog input.</exception>
        /// <exception cref="System.InvalidOperationException">The pin is already open.</exception>
        public AnalogInputPin OpenPin(int pinNumber)
        {
            if (!SupportsAnalogInput(pinNumber))
            {
                throw new System.NotSupportedException("Pin " + pinNumber + " is not supporting analog input.");
            }
            if (IndexOfOpenPin(pinNumber) >= 0)
            {
                throw new System.InvalidOperationException("The selected pin is already open.");
            }
            AnalogInputPin openPin = OpenPinCore(pinNumber);
            Add(openPin);
            return openPin;
        }

        /// <summary>Opens a pin. The overridable half of <see cref="OpenPin(int)"/>: it runs after the
        /// support and already-open checks, so an implementation constructs and returns the pin.</summary>
        protected abstract AnalogInputPin OpenPinCore(int pinNumber);

        /// <summary>Whether the given pin is currently open.</summary>
        public virtual bool IsPinOpen(int pinNumber)
        {
            return IndexOfOpenPin(pinNumber) >= 0;
        }

        /// <summary>Closes an open pin, disposing it.</summary>
        /// <remarks>Closing a pin this controller did not open is a no-op, matching upstream, whose
        /// `List.Remove` simply answers false. <b>The pin leaves the open list BEFORE it is
        /// disposed</b>, because <see cref="AnalogInputPin.Dispose()"/> calls back here -- upstream is
        /// saved from the same recursion by `Remove` answering false the second time, and this is that
        /// guard written where it can be seen. It bounds the recursion; it does not make disposal
        /// single, and upstream's is not single either -- a pin disposed by a caller has its
        /// <c>Dispose(bool)</c> run twice on both runtimes.</remarks>
        public virtual void ClosePin(AnalogInputPin pin)
        {
            for (int i = 0; i < _openCount; i++)
            {
                if (object.ReferenceEquals(_openPins[i], pin))
                {
                    RemoveAt(i);
                    pin.Dispose();
                    return;
                }
            }
        }

        private int IndexOfOpenPin(int pinNumber)
        {
            for (int i = 0; i < _openCount; i++)
            {
                if (_openPins[i].PinNumber == pinNumber)
                {
                    return i;
                }
            }
            return -1;
        }

        private void Add(AnalogInputPin pin)
        {
            if (_openPins == null)
            {
                _openPins = new AnalogInputPin[4];
            }
            else if (_openCount == _openPins.Length)
            {
                AnalogInputPin[] grown = new AnalogInputPin[_openPins.Length * 2];
                for (int i = 0; i < _openCount; i++)
                {
                    grown[i] = _openPins[i];
                }
                _openPins = grown;
            }
            _openPins[_openCount] = pin;
            _openCount++;
        }

        private void RemoveAt(int index)
        {
            for (int i = index + 1; i < _openCount; i++)
            {
                _openPins[i - 1] = _openPins[i];
            }
            _openCount--;
            _openPins[_openCount] = null;
        }

        /// <summary>Disposes this controller, closing every pin it still holds open.</summary>
        protected virtual void Dispose(bool disposing)
        {
            if (_openCount == 0)
            {
                return;
            }
            AnalogInputPin[] copy = new AnalogInputPin[_openCount];
            for (int i = 0; i < copy.Length; i++)
            {
                copy[i] = _openPins[i];
            }
            for (int i = 0; i < copy.Length; i++)
            {
                copy[i].Dispose();
            }
            for (int i = 0; i < _openCount; i++)
            {
                _openPins[i] = null;
            }
            _openCount = 0;
        }

        /// <summary>Disposes this controller.</summary>
        public void Dispose()
        {
            Dispose(true);
        }
    }
}
