// Lamella managed corlib (from scratch). -- System.Byte
using System.Globalization;

namespace System
{
    public struct Byte : IComparable, IFormattable
    {
        public const byte MaxValue = 255;
        public const byte MinValue = 0;

        public override string ToString() { return NumberFormatter.Decimal(this, 0); }

        public string ToString(string format) { return NumberFormatter.Format(this, 2, format); }

        public string ToString(IFormatProvider provider) { NumberFormatter.Ask(false, null, provider); return ToString(); }
        public string ToString(string format, IFormatProvider provider) { NumberFormatter.Ask(false, format, provider); return NumberFormatter.Format(this, 2, format); }

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
            if (!(obj is byte)) throw new ArgumentException("Object must be of type Byte.");
            return CompareTo((byte)obj);
        }

        public static byte Parse(string s)
        {
            if ((object)s == null) throw new ArgumentNullException("s");
            ulong bits;
            int outcome = NumberParser.ScanWholeNumber(s, (int)NumberStyles.Integer, 2, 255UL, 0UL, out bits);
            if (outcome != NumberParser.Done) NumberParser.Refuse(outcome, s, "Value was either too large or too small for an unsigned byte.");
            return (byte)bits;
        }

        public static byte Parse(string s, NumberStyles style)
        {
            if ((object)s == null) throw new ArgumentNullException("s");
            NumberParser.CheckWholeNumberStyle(style);
            ulong bits;
            int outcome = NumberParser.ScanWholeNumber(s, (int)style, 2, 255UL, 0UL, out bits);
            if (outcome != NumberParser.Done) NumberParser.Refuse(outcome, s, "Value was either too large or too small for an unsigned byte.");
            return (byte)bits;
        }

        public static byte Parse(string s, IFormatProvider provider)
        {
            return Parse(s, NumberStyles.Integer, provider);
        }

        public static byte Parse(string s, NumberStyles style, IFormatProvider provider)
        {
            if ((object)s == null) throw new ArgumentNullException("s");
            NumberParser.CheckWholeNumberStyle(style);
            NumberParser.AskProvider(provider);
            ulong bits;
            int outcome = NumberParser.ScanWholeNumber(s, (int)style, 2, 255UL, 0UL, out bits);
            if (outcome != NumberParser.Done) NumberParser.Refuse(outcome, s, "Value was either too large or too small for an unsigned byte.");
            return (byte)bits;
        }

#if LAMELLA_SURFACE_NETFX_2_0
        public static bool TryParse(string s, out byte result)
        {
            result = 0;
            if ((object)s == null) return false;
            ulong bits;
            if (NumberParser.ScanWholeNumber(s, (int)NumberStyles.Integer, 2, 255UL, 0UL, out bits) != NumberParser.Done) return false;
            result = (byte)bits;
            return true;
        }

        public static bool TryParse(string s, NumberStyles style, IFormatProvider provider, out byte result)
        {
            NumberParser.CheckWholeNumberStyle(style);
            result = 0;
            if ((object)s == null) return false;
            NumberParser.AskProvider(provider);
            ulong bits;
            if (NumberParser.ScanWholeNumber(s, (int)style, 2, 255UL, 0UL, out bits) != NumberParser.Done) return false;
            result = (byte)bits;
            return true;
        }
#endif
    }
}
