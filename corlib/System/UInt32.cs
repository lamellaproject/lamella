// Lamella managed corlib (from scratch). -- System.UInt32
using System.Globalization;

namespace System
{
    public struct UInt32 : IComparable, IFormattable
    {
        public const uint MaxValue = 4294967295;
        public const uint MinValue = 0;

        public override string ToString()
        {
            long value = this;
            if (value == 0) return "0";
            char[] buffer = new char[16];
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

        public string ToString(string format) { return NumberFormatter.Format(this, 8, format); }
        public string ToString(IFormatProvider provider) { NumberFormatter.Ask(false, null, provider); return ToString(); }
        public string ToString(string format, IFormatProvider provider) { NumberFormatter.Ask(false, format, provider); return NumberFormatter.Format(this, 8, format); }

        public bool Equals(uint obj) { return this == obj; }

        public override bool Equals(object obj)
        {
            if (obj is uint) return this == (uint)obj;
            return false;
        }

        public override int GetHashCode() { return (int)this; }

        public int CompareTo(uint value)
        {
            if (this < value) return -1;
            if (this > value) return 1;
            return 0;
        }

        public int CompareTo(object obj)
        {
            if (obj == null) return 1;
            if (!(obj is uint)) throw new ArgumentException("Object must be of type UInt32.");
            return CompareTo((uint)obj);
        }

        public static uint Parse(string s)
        {
            if ((object)s == null) throw new ArgumentNullException("s");
            ulong bits;
            int outcome = NumberParser.ScanWholeNumber(s, (int)NumberStyles.Integer, 8, 4294967295UL, 0UL, out bits);
            if (outcome != NumberParser.Done) NumberParser.Refuse(outcome, s, "Value was either too large or too small for a UInt32.");
            return (uint)bits;
        }

        public static uint Parse(string s, NumberStyles style)
        {
            if ((object)s == null) throw new ArgumentNullException("s");
            NumberParser.CheckWholeNumberStyle(style);
            ulong bits;
            int outcome = NumberParser.ScanWholeNumber(s, (int)style, 8, 4294967295UL, 0UL, out bits);
            if (outcome != NumberParser.Done) NumberParser.Refuse(outcome, s, "Value was either too large or too small for a UInt32.");
            return (uint)bits;
        }

        public static uint Parse(string s, IFormatProvider provider)
        {
            return Parse(s, NumberStyles.Integer, provider);
        }

        public static uint Parse(string s, NumberStyles style, IFormatProvider provider)
        {
            if ((object)s == null) throw new ArgumentNullException("s");
            NumberParser.CheckWholeNumberStyle(style);
            NumberParser.AskProvider(provider);
            ulong bits;
            int outcome = NumberParser.ScanWholeNumber(s, (int)style, 8, 4294967295UL, 0UL, out bits);
            if (outcome != NumberParser.Done) NumberParser.Refuse(outcome, s, "Value was either too large or too small for a UInt32.");
            return (uint)bits;
        }

#if LAMELLA_SURFACE_NETFX_2_0
        public static bool TryParse(string s, out uint result)
        {
            result = 0;
            if ((object)s == null) return false;
            ulong bits;
            if (NumberParser.ScanWholeNumber(s, (int)NumberStyles.Integer, 8, 4294967295UL, 0UL, out bits) != NumberParser.Done) return false;
            result = (uint)bits;
            return true;
        }

        public static bool TryParse(string s, NumberStyles style, IFormatProvider provider, out uint result)
        {
            NumberParser.CheckWholeNumberStyle(style);
            result = 0;
            if ((object)s == null) return false;
            NumberParser.AskProvider(provider);
            ulong bits;
            if (NumberParser.ScanWholeNumber(s, (int)style, 8, 4294967295UL, 0UL, out bits) != NumberParser.Done) return false;
            result = (uint)bits;
            return true;
        }
#endif
    }
}
