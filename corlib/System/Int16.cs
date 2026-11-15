// Lamella managed corlib (from scratch). -- System.Int16
using System.Globalization;

namespace System
{
    public struct Int16 : IComparable, IFormattable
    {
        public const short MaxValue = 32767;
        public const short MinValue = -32768;

        public override string ToString()
        {
            int value = this;
            if (value == 0) return "0";
            bool negative = value < 0;
            int n = negative ? value : -value;
            char[] buffer = new char[8];
            int pos = buffer.Length;
            while (n != 0)
            {
                int digit = -(n % 10);
                pos = pos - 1;
                buffer[pos] = (char)('0' + digit);
                n = n / 10;
            }
            System.Text.StringBuilder result = new System.Text.StringBuilder();
            if (negative) result.Append('-');
            for (int i = pos; i < buffer.Length; i++) result.Append(buffer[i]);
            return result.ToString();
        }

        public string ToString(string format) { return NumberFormatter.Format(this, 4, format); }
        public string ToString(IFormatProvider provider) { NumberFormatter.Ask(this < 0, null, provider); return ToString(); }
        public string ToString(string format, IFormatProvider provider) { NumberFormatter.Ask(this < 0, format, provider); return NumberFormatter.Format(this, 4, format); }

        public bool Equals(short obj) { return this == obj; }

        public override bool Equals(object obj)
        {
            if (obj is short) return this == (short)obj;
            return false;
        }

        public override int GetHashCode()
        {
            int value = this;
            return value ^ (value << 16);
        }

        public int CompareTo(short value)
        {
            if (this < value) return -1;
            if (this > value) return 1;
            return 0;
        }

        public int CompareTo(object obj)
        {
            if (obj == null) return 1;
            if (!(obj is short)) throw new ArgumentException("Object must be of type Int16.");
            return CompareTo((short)obj);
        }

        public static short Parse(string s)
        {
            if ((object)s == null) throw new ArgumentNullException("s");
            ulong bits;
            int outcome = NumberParser.ScanWholeNumber(s, (int)NumberStyles.Integer, 4, 32767UL, 32768UL, out bits);
            if (outcome != NumberParser.Done) NumberParser.Refuse(outcome, s, "Value was either too large or too small for an Int16.");
            return (short)bits;
        }

        public static short Parse(string s, NumberStyles style)
        {
            if ((object)s == null) throw new ArgumentNullException("s");
            NumberParser.CheckWholeNumberStyle(style);
            ulong bits;
            int outcome = NumberParser.ScanWholeNumber(s, (int)style, 4, 32767UL, 32768UL, out bits);
            if (outcome != NumberParser.Done) NumberParser.Refuse(outcome, s, "Value was either too large or too small for an Int16.");
            return (short)bits;
        }

        public static short Parse(string s, IFormatProvider provider)
        {
            return Parse(s, NumberStyles.Integer, provider);
        }

        public static short Parse(string s, NumberStyles style, IFormatProvider provider)
        {
            if ((object)s == null) throw new ArgumentNullException("s");
            NumberParser.CheckWholeNumberStyle(style);
            NumberParser.AskProvider(provider);
            ulong bits;
            int outcome = NumberParser.ScanWholeNumber(s, (int)style, 4, 32767UL, 32768UL, out bits);
            if (outcome != NumberParser.Done) NumberParser.Refuse(outcome, s, "Value was either too large or too small for an Int16.");
            return (short)bits;
        }

#if LAMELLA_SURFACE_NETFX_2_0
        public static bool TryParse(string s, out short result)
        {
            result = 0;
            if ((object)s == null) return false;
            ulong bits;
            if (NumberParser.ScanWholeNumber(s, (int)NumberStyles.Integer, 4, 32767UL, 32768UL, out bits) != NumberParser.Done) return false;
            result = (short)bits;
            return true;
        }

        public static bool TryParse(string s, NumberStyles style, IFormatProvider provider, out short result)
        {
            NumberParser.CheckWholeNumberStyle(style);
            result = 0;
            if ((object)s == null) return false;
            NumberParser.AskProvider(provider);
            ulong bits;
            if (NumberParser.ScanWholeNumber(s, (int)style, 4, 32767UL, 32768UL, out bits) != NumberParser.Done) return false;
            result = (short)bits;
            return true;
        }
#endif
    }
}
