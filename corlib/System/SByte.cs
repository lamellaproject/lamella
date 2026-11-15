// Lamella managed corlib (from scratch). -- System.SByte
using System.Globalization;

namespace System
{
    public struct SByte : IComparable, IFormattable
    {
        public const sbyte MaxValue = 127;
        public const sbyte MinValue = -128;

        public override string ToString() { return NumberFormatter.Decimal(this, 0); }

        public string ToString(string format) { return NumberFormatter.Format(this, 2, format); }

        public string ToString(IFormatProvider provider) { NumberFormatter.Ask(this < 0, null, provider); return ToString(); }
        public string ToString(string format, IFormatProvider provider) { NumberFormatter.Ask(this < 0, format, provider); return NumberFormatter.Format(this, 2, format); }

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
            if (!(obj is sbyte)) throw new ArgumentException("Object must be of type SByte.");
            return CompareTo((sbyte)obj);
        }

        public static sbyte Parse(string s)
        {
            if ((object)s == null) throw new ArgumentNullException("s");
            ulong bits;
            int outcome = NumberParser.ScanWholeNumber(s, (int)NumberStyles.Integer, 2, 127UL, 128UL, out bits);
            if (outcome != NumberParser.Done) NumberParser.Refuse(outcome, s, "Value was either too large or too small for a signed byte.");
            return (sbyte)bits;
        }

        public static sbyte Parse(string s, NumberStyles style)
        {
            if ((object)s == null) throw new ArgumentNullException("s");
            NumberParser.CheckWholeNumberStyle(style);
            ulong bits;
            int outcome = NumberParser.ScanWholeNumber(s, (int)style, 2, 127UL, 128UL, out bits);
            if (outcome != NumberParser.Done) NumberParser.Refuse(outcome, s, "Value was either too large or too small for a signed byte.");
            return (sbyte)bits;
        }

        public static sbyte Parse(string s, IFormatProvider provider)
        {
            return Parse(s, NumberStyles.Integer, provider);
        }

        public static sbyte Parse(string s, NumberStyles style, IFormatProvider provider)
        {
            if ((object)s == null) throw new ArgumentNullException("s");
            NumberParser.CheckWholeNumberStyle(style);
            NumberParser.AskProvider(provider);
            ulong bits;
            int outcome = NumberParser.ScanWholeNumber(s, (int)style, 2, 127UL, 128UL, out bits);
            if (outcome != NumberParser.Done) NumberParser.Refuse(outcome, s, "Value was either too large or too small for a signed byte.");
            return (sbyte)bits;
        }

#if LAMELLA_SURFACE_NETFX_2_0
        public static bool TryParse(string s, out sbyte result)
        {
            result = 0;
            if ((object)s == null) return false;
            ulong bits;
            if (NumberParser.ScanWholeNumber(s, (int)NumberStyles.Integer, 2, 127UL, 128UL, out bits) != NumberParser.Done) return false;
            result = (sbyte)bits;
            return true;
        }

        public static bool TryParse(string s, NumberStyles style, IFormatProvider provider, out sbyte result)
        {
            NumberParser.CheckWholeNumberStyle(style);
            result = 0;
            if ((object)s == null) return false;
            NumberParser.AskProvider(provider);
            ulong bits;
            if (NumberParser.ScanWholeNumber(s, (int)style, 2, 127UL, 128UL, out bits) != NumberParser.Done) return false;
            result = (sbyte)bits;
            return true;
        }
#endif
    }
}
