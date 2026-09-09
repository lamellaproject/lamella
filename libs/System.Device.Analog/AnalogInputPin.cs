// System.Device.Analog -- the dotnet/iot analog input PIN: abstract AnalogInputPin : IDisposable
namespace System.Device.Analog
{
    /// <summary>Driver for analog input pins.</summary>
    /// <remarks>
    /// <para>A platform derives from this and supplies <see cref="ReadRaw"/> and
    /// <see cref="AdcResolutionBits"/>; a program obtains one from
    /// <see cref="AnalogController.OpenPin(int)"/> rather than constructing it.</para>
    /// <para><b>This is a strict subset of upstream's surface, not a variation on it.</b> Everything
    /// here matches dotnet/iot member for member, and what is absent is absent rather than
    /// respelled -- so a program written against these members compiles and runs unchanged on
    /// dotnet/iot.</para>
    /// </remarks>
    public abstract class AnalogInputPin : System.IDisposable
    {

        /// <summary>Constructs an instance of an analog pin. Not usually called directly: use
        /// <see cref="AnalogController.OpenPin(int)"/> instead.</summary>
        public AnalogInputPin(AnalogController controller, int pinNumber)
        {
            _controller = controller;
            _pinNumber = pinNumber;
        }

        private readonly AnalogController _controller;
        private readonly int _pinNumber;

        /// <summary>The controller this pin belongs to.</summary>
        protected AnalogController Controller { get { return _controller; } }

        /// <summary>The logical pin number of this instance.</summary>
        public int PinNumber { get { return _pinNumber; } }

        /// <summary>Resolution of the analog-to-digital converter in bits. If the converter reports
        /// negative values, the sign bit is counted too.</summary>
        public abstract int AdcResolutionBits { get; }

        /// <summary>Reads a raw value from the pin. The scale depends on the hardware; see
        /// <see cref="AdcResolutionBits"/>.</summary>
        public abstract uint ReadRaw();

        /// <summary>Disposes this pin, closing it on its controller.</summary>
        protected virtual void Dispose(bool disposing)
        {
            if (disposing)
            {
                Controller.ClosePin(this);
            }
        }

        /// <summary>Disposes this pin.</summary>
        public void Dispose()
        {
            Dispose(true);
        }
    }
}
