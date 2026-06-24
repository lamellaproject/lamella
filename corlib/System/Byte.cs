// Lamella managed corlib (from scratch). -- System.Byte
namespace System
{
    public struct Byte : IComparable
    {
        public const byte MaxValue = 255;
        public const byte MinValue = 0;

        public override string ToString() { return NumberFormatter.Format(this, 2, null); }

        public string ToString(string format) { return NumberFormatter.Format(this, 2, format); }

        public static byte Parse(string s)
        {
            int value = Int32.Parse(s);
            if (value < MinValue || value > MaxValue) throw new OverflowException("Value was either too large or too small for an unsigned byte.");
            return (byte)value;
        }

        public static bool TryParse(string s, out byte result)
        {
            int value;
            if (Int32.TryParse(s, out value) && value >= MinValue && value <= MaxValue)
            {
                result = (byte)value;
                return true;
            }
            result = 0;
            return false;
        }

        public bool Equals(byte obj) { return this == obj; }

        public override bool Equals(object obj)
        {
            if (obj is byte) return this == (byte)obj;
            return false;
        }

        public override int GetHashCode() { return this; }

        public int CompareTo(byte value)
        {
            if (this < value) return -1;
            if (this > value) return 1;
            return 0;
        }

        public int CompareTo(object obj)
        {
            if (obj == null) return 1;
            return CompareTo((byte)obj);
        }
    }
}
