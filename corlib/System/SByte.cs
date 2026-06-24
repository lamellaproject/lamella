// Lamella managed corlib (from scratch). -- System.SByte
namespace System
{
    public struct SByte : IComparable
    {
        public const sbyte MaxValue = 127;
        public const sbyte MinValue = -128;

        public override string ToString() { return NumberFormatter.Format(this, 2, null); }

        public string ToString(string format) { return NumberFormatter.Format(this, 2, format); }

        public static sbyte Parse(string s)
        {
            int value = Int32.Parse(s);
            if (value < MinValue || value > MaxValue) throw new OverflowException("Value was either too large or too small for a signed byte.");
            return (sbyte)value;
        }

        public static bool TryParse(string s, out sbyte result)
        {
            int value;
            if (Int32.TryParse(s, out value) && value >= MinValue && value <= MaxValue)
            {
                result = (sbyte)value;
                return true;
            }
            result = 0;
            return false;
        }

        public bool Equals(sbyte obj) { return this == obj; }

        public override bool Equals(object obj)
        {
            if (obj is sbyte) return this == (sbyte)obj;
            return false;
        }

        public override int GetHashCode() { int value = this; return value ^ (value << 8); }

        public int CompareTo(sbyte value)
        {
            if (this < value) return -1;
            if (this > value) return 1;
            return 0;
        }

        public int CompareTo(object obj)
        {
            if (obj == null) return 1;
            return CompareTo((sbyte)obj);
        }
    }
}
