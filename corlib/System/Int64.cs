// Lamella managed corlib (from scratch). -- System.Int64
using System.Globalization;

namespace System
{
    public struct Int64 : IComparable, IFormattable
    {
        public const long MaxValue = 9223372036854775807;
        public const long MinValue = -9223372036854775808;

        public override bool Equals(object obj)
        {
            if (obj is long) return this == (long)obj;
            return false;
        }

        public bool Equals(long obj) { return this == obj; }

        public override int GetHashCode()
        {
            long value = this;
            return (int)value ^ (int)(value >> 32);
        }

        public int CompareTo(long value)
        {
            if (this < value) return -1;
            if (this > value) return 1;
            return 0;
        }

        public int CompareTo(object obj)
        {
            if (obj == null) return 1;
            if (!(obj is long)) throw new ArgumentException("Object must be of type Int64.");
            return CompareTo((long)obj);
        }

        public override string ToString()
        {
            long value = this;
            if (value == 0) return "0";
            bool negative = value < 0;
            long n = negative ? value : -value;
            char[] buffer = new char[24];
            int pos = buffer.Length;
            while (n != 0)
            {
                int digit = (int)(-(n % 10));
                pos = pos - 1;
                buffer[pos] = (char)('0' + digit);
                n = n / 10;
            }
            System.Text.StringBuilder result = new System.Text.StringBuilder();
            if (negative) result.Append('-');
            for (int i = pos; i < buffer.Length; i++) result.Append(buffer[i]);
            return result.ToString();
        }

        public string ToString(string format)
        {
            return NumberFormatter.Format(this, 16, format);
        }

        public string ToString(IFormatProvider provider) { NumberFormatter.Ask(this < 0, null, provider); return ToString(); }

        public string ToString(string format, IFormatProvider provider)
        {
            NumberFormatter.Ask(this < 0, format, provider);
            return NumberFormatter.Format(this, 16, format);
        }

        public static long Parse(string s)
        {
            if ((object)s == null) throw new ArgumentNullException("s");
            ulong bits;
            int outcome = NumberParser.ScanWholeNumber(s, (int)NumberStyles.Integer, 16, 9223372036854775807UL, 9223372036854775808UL, out bits);
            if (outcome != NumberParser.Done) NumberParser.Refuse(outcome, s, "Value was either too large or too small for an Int64.");
            return (long)bits;
        }

        public static long Parse(string s, NumberStyles style)
        {
            if ((object)s == null) throw new ArgumentNullException("s");
            NumberParser.CheckWholeNumberStyle(style);
            ulong bits;
            int outcome = NumberParser.ScanWholeNumber(s, (int)style, 16, 9223372036854775807UL, 9223372036854775808UL, out bits);
            if (outcome != NumberParser.Done) NumberParser.Refuse(outcome, s, "Value was either too large or too small for an Int64.");
            return (long)bits;
        }

        public static long Parse(string s, IFormatProvider provider)
        {
            return Parse(s, NumberStyles.Integer, provider);
        }

        public static long Parse(string s, NumberStyles style, IFormatProvider provider)
        {
            if ((object)s == null) throw new ArgumentNullException("s");
            NumberParser.CheckWholeNumberStyle(style);
            NumberParser.AskProvider(provider);
            ulong bits;
            int outcome = NumberParser.ScanWholeNumber(s, (int)style, 16, 9223372036854775807UL, 9223372036854775808UL, out bits);
            if (outcome != NumberParser.Done) NumberParser.Refuse(outcome, s, "Value was either too large or too small for an Int64.");
            return (long)bits;
        }

#if LAMELLA_SURFACE_NETFX_2_0
        public static bool TryParse(string s, out long result)
        {
            result = 0;
            if ((object)s == null) return false;
            ulong bits;
            if (NumberParser.ScanWholeNumber(s, (int)NumberStyles.Integer, 16, 9223372036854775807UL, 9223372036854775808UL, out bits) != NumberParser.Done) return false;
            result = (long)bits;
            return true;
        }

        public static bool TryParse(string s, NumberStyles style, IFormatProvider provider, out long result)
        {
            NumberParser.CheckWholeNumberStyle(style);
            result = 0;
            if ((object)s == null) return false;
            NumberParser.AskProvider(provider);
            ulong bits;
            if (NumberParser.ScanWholeNumber(s, (int)style, 16, 9223372036854775807UL, 9223372036854775808UL, out bits) != NumberParser.Done) return false;
            result = (long)bits;
            return true;
        }
#endif
    }
}
