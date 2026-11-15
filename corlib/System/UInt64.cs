// Lamella managed corlib (from scratch). -- System.UInt64
using System.Globalization;

namespace System
{
    public struct UInt64 : IComparable, IFormattable
    {
        public const ulong MaxValue = 18446744073709551615;
        public const ulong MinValue = 0;

        public override string ToString()
        {
            ulong value = this;
            if (value == 0) return "0";
            char[] buffer = new char[24];
            int pos = buffer.Length;
            while (value != 0)
            {
                int digit = (int)(value % 10);
                pos = pos - 1;
                buffer[pos] = (char)('0' + digit);
                value = value / 10;
            }
            System.Text.StringBuilder result = new System.Text.StringBuilder();
            for (int i = pos; i < buffer.Length; i++) result.Append(buffer[i]);
            return result.ToString();
        }

        public string ToString(string format) { return NumberFormatter.FormatUnsigned(this, format); }
        public string ToString(IFormatProvider provider) { NumberFormatter.Ask(false, null, provider); return ToString(); }
        public string ToString(string format, IFormatProvider provider) { NumberFormatter.Ask(false, format, provider); return NumberFormatter.FormatUnsigned(this, format); }

        public bool Equals(ulong obj) { return this == obj; }

        public override bool Equals(object obj)
        {
            if (obj is ulong) return this == (ulong)obj;
            return false;
        }

        public override int GetHashCode()
        {
            ulong value = this;
            return (int)value ^ (int)(value >> 32);
        }

        public int CompareTo(ulong value)
        {
            if (this < value) return -1;
            if (this > value) return 1;
            return 0;
        }

        public int CompareTo(object obj)
        {
            if (obj == null) return 1;
            if (!(obj is ulong)) throw new ArgumentException("Object must be of type UInt64.");
            return CompareTo((ulong)obj);
        }

        public static ulong Parse(string s)
        {
            if ((object)s == null) throw new ArgumentNullException("s");
            ulong bits;
            int outcome = NumberParser.ScanWholeNumber(s, (int)NumberStyles.Integer, 16, 18446744073709551615UL, 0UL, out bits);
            if (outcome != NumberParser.Done) NumberParser.Refuse(outcome, s, "Value was either too large or too small for a UInt64.");
            return (ulong)bits;
        }

        public static ulong Parse(string s, NumberStyles style)
        {
            if ((object)s == null) throw new ArgumentNullException("s");
            NumberParser.CheckWholeNumberStyle(style);
            ulong bits;
            int outcome = NumberParser.ScanWholeNumber(s, (int)style, 16, 18446744073709551615UL, 0UL, out bits);
            if (outcome != NumberParser.Done) NumberParser.Refuse(outcome, s, "Value was either too large or too small for a UInt64.");
            return (ulong)bits;
        }

        public static ulong Parse(string s, IFormatProvider provider)
        {
            return Parse(s, NumberStyles.Integer, provider);
        }

        public static ulong Parse(string s, NumberStyles style, IFormatProvider provider)
        {
            if ((object)s == null) throw new ArgumentNullException("s");
            NumberParser.CheckWholeNumberStyle(style);
            NumberParser.AskProvider(provider);
            ulong bits;
            int outcome = NumberParser.ScanWholeNumber(s, (int)style, 16, 18446744073709551615UL, 0UL, out bits);
            if (outcome != NumberParser.Done) NumberParser.Refuse(outcome, s, "Value was either too large or too small for a UInt64.");
            return (ulong)bits;
        }

#if LAMELLA_SURFACE_NETFX_2_0
        public static bool TryParse(string s, out ulong result)
        {
            result = 0;
            if ((object)s == null) return false;
            ulong bits;
            if (NumberParser.ScanWholeNumber(s, (int)NumberStyles.Integer, 16, 18446744073709551615UL, 0UL, out bits) != NumberParser.Done) return false;
            result = (ulong)bits;
            return true;
        }

        public static bool TryParse(string s, NumberStyles style, IFormatProvider provider, out ulong result)
        {
            NumberParser.CheckWholeNumberStyle(style);
            result = 0;
            if ((object)s == null) return false;
            NumberParser.AskProvider(provider);
            ulong bits;
            if (NumberParser.ScanWholeNumber(s, (int)style, 16, 18446744073709551615UL, 0UL, out bits) != NumberParser.Done) return false;
            result = (ulong)bits;
            return true;
        }
#endif
    }
}
