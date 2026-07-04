// Lamella System.Device.Gpio -- a separate assembly matching Microsoft's dotnet/iot GPIO API.
namespace System.Device.Gpio
{
    /// <summary>Represents the value of a pin: <see cref="High"/> or <see cref="Low"/>.</summary>
    public struct PinValue
    {
        private readonly byte _value;

        private PinValue(byte value) { _value = value; }

        /// <summary>The value of the pin is low (0).</summary>
        public static PinValue Low { get { return new PinValue(0); } }

        /// <summary>The value of the pin is high (1).</summary>
        public static PinValue High { get { return new PinValue(1); } }

        /// <summary>0 means <see cref="Low"/>; any other value means <see cref="High"/>.</summary>
        public static implicit operator PinValue(int value) { return value == 0 ? Low : High; }

        /// <summary><c>false</c> means <see cref="Low"/>; <c>true</c> means <see cref="High"/>.</summary>
        public static implicit operator PinValue(bool value) { return value ? High : Low; }

        /// <summary>Returns 0 on <see cref="Low"/>, 1 on <see cref="High"/>.</summary>
        public static explicit operator byte(PinValue value) { return value._value; }

        /// <summary>Returns 0 on <see cref="Low"/>, 1 on <see cref="High"/>.</summary>
        public static explicit operator int(PinValue value) { return value._value; }

        /// <summary>Returns <c>false</c> on <see cref="Low"/>, <c>true</c> on <see cref="High"/>.</summary>
        public static explicit operator bool(PinValue value) { return value._value != 0; }

        /// <summary>Equality operator.</summary>
        public static bool operator ==(PinValue a, PinValue b) { return a._value == b._value; }

        /// <summary>Inequality operator.</summary>
        public static bool operator !=(PinValue a, PinValue b) { return a._value != b._value; }

        /// <summary>Returns the inverse of the value.</summary>
        public static PinValue operator !(PinValue value) { return value._value == 0 ? High : Low; }

        /// <summary>Indicates whether this instance and a specified object are equal.</summary>
        public override bool Equals(object obj) { return (obj is PinValue) && (this == (PinValue)obj); }

        /// <summary>Returns the hash code for this instance.</summary>
        public override int GetHashCode() { return _value; }

        /// <summary>Returns "Low" for <see cref="Low"/> and "High" for <see cref="High"/>.</summary>
        public override string ToString() { return _value == 0 ? "Low" : "High"; }
    }
}
