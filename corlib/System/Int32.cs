// Lamella managed corlib (from scratch). -- System.Int32
using System.Globalization;

namespace System
{
    public struct Int32 : IComparable, IFormattable
    {
        public const int MaxValue = 2147483647;
        public const int MinValue = -2147483648;

        public override bool Equals(object obj)
        {
            if (obj is int) return this == (int)obj;
            return false;
        }

        public bool Equals(int obj) { return this == obj; }

        public override int GetHashCode() { return this; }

        public int CompareTo(int value)
        {
            if (this < value) return -1;
            if (this > value) return 1;
            return 0;
        }

        public int CompareTo(object obj)
        {
            if (obj == null) return 1;
            if (!(obj is int)) throw new ArgumentException("Object must be of type Int32.");
            return CompareTo((int)obj);
        }

        public override string ToString()
        {
            return Decimal(this);
        }

        internal static string Decimal(int value)
        {
            if (value == 0) return "0";
            bool negative = value < 0;
            int n = negative ? value : -value;
            char[] buffer = new char[16];
            int pos = buffer.Length;
            while (n != 0)
            {
                int digit = -(n % 10);
                pos = pos - 1;
                buffer[pos] = (char)('0' + digit);
                n = n / 10;
            }
            if (negative)
            {
                pos = pos - 1;
                buffer[pos] = '-';
            }
            return new string(buffer, pos, buffer.Length - pos);
        }

        public string ToString(string format)
        {
            return NumberFormatter.Format((long)this, 8, format);
        }

        public string ToString(IFormatProvider provider) { NumberFormatter.Ask(this < 0, null, provider); return Decimal(this); }

        public string ToString(string format, IFormatProvider provider)
        {
            NumberFormatter.Ask(this < 0, format, provider);
            return NumberFormatter.Format((long)this, 8, format);
        }

        public static int Parse(string s)
        {
            if ((object)s == null) throw new ArgumentNullException("s");
            ulong bits;
            int outcome = NumberParser.ScanWholeNumber(s, (int)NumberStyles.Integer, 8, 2147483647UL, 2147483648UL, out bits);
            if (outcome != NumberParser.Done) NumberParser.Refuse(outcome, s, "Value was either too large or too small for an Int32.");
            return (int)bits;
        }

        public static int Parse(string s, NumberStyles style)
        {
            if ((object)s == null) throw new ArgumentNullException("s");
            NumberParser.CheckWholeNumberStyle(style);
            ulong bits;
            int outcome = NumberParser.ScanWholeNumber(s, (int)style, 8, 2147483647UL, 2147483648UL, out bits);
            if (outcome != NumberParser.Done) NumberParser.Refuse(outcome, s, "Value was either too large or too small for an Int32.");
            return (int)bits;
        }

        public static int Parse(string s, IFormatProvider provider)
        {
            return Parse(s, NumberStyles.Integer, provider);
        }

        public static int Parse(string s, NumberStyles style, IFormatProvider provider)
        {
            if ((object)s == null) throw new ArgumentNullException("s");
            NumberParser.CheckWholeNumberStyle(style);
            NumberParser.AskProvider(provider);
            ulong bits;
            int outcome = NumberParser.ScanWholeNumber(s, (int)style, 8, 2147483647UL, 2147483648UL, out bits);
            if (outcome != NumberParser.Done) NumberParser.Refuse(outcome, s, "Value was either too large or too small for an Int32.");
            return (int)bits;
        }

#if LAMELLA_SURFACE_NETFX_2_0
        public static bool TryParse(string s, out int result)
        {
            result = 0;
            if ((object)s == null) return false;
            ulong bits;
            if (NumberParser.ScanWholeNumber(s, (int)NumberStyles.Integer, 8, 2147483647UL, 2147483648UL, out bits) != NumberParser.Done) return false;
            result = (int)bits;
            return true;
        }

        public static bool TryParse(string s, NumberStyles style, IFormatProvider provider, out int result)
        {
            NumberParser.CheckWholeNumberStyle(style);
            result = 0;
            if ((object)s == null) return false;
            NumberParser.AskProvider(provider);
            ulong bits;
            if (NumberParser.ScanWholeNumber(s, (int)style, 8, 2147483647UL, 2147483648UL, out bits) != NumberParser.Done) return false;
            result = (int)bits;
            return true;
        }
#endif
    }
}
