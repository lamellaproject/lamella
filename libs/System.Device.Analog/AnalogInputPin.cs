// System.Device.Analog -- the dotnet/iot analog input PIN: abstract AnalogInputPin : IDisposable
namespace System.Device.Analog
{
    /// <summary>Driver for analog input pins.</summary>
    /// <remarks>
    /// <para>A Lamella platform derives from this and supplies <see cref="ReadRaw"/> and
    /// <see cref="AdcResolutionBits"/>; a program obtains one from
    /// <see cref="AnalogController.OpenPin(int)"/> rather than constructing it.</para>
    /// <para><b>A subset of dotnet/iot's <c>AnalogInputPin</c>.</b> Every member here has dotnet/iot's
    /// signature, so a program that opens a pin with <see cref="AnalogController.OpenPin(int)"/> and
    /// reads it compiles unchanged on dotnet/iot. Three things are absent: every member typed in
    /// UnitsNet (<c>VoltageReference</c>, <c>MinVoltage</c>, <c>MaxVoltage</c>, <c>ReadVoltage</c> and
    /// <c>ConvertToVoltage</c>); the <c>ValueChanged</c> event and the methods that enable, disable
    /// and raise it, whose arguments carry a UnitsNet value; and a public constructor. dotnet/iot's
    /// constructor takes a UnitsNet <c>ElectricPotential</c>, so code outside Lamella cannot subclass
    /// this pin until that type is available.</para>
    /// </remarks>
    public abstract class AnalogInputPin : System.IDisposable
    {

        /// <summary>Constructs an instance of an analog pin. A program does not call it: it opens a
        /// pin with <see cref="AnalogController.OpenPin(int)"/>.</summary>
        internal AnalogInputPin(AnalogController controller, int pinNumber)
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
