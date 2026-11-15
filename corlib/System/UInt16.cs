// Lamella managed corlib (from scratch). -- System.UInt16
using System.Globalization;

namespace System
{
    public struct UInt16 : IComparable, IFormattable
    {
        public const ushort MaxValue = 65535;
        public const ushort MinValue = 0;

        public override string ToString()
        {
            int value = this;
            if (value == 0) return "0";
            char[] buffer = new char[8];
            int pos = buffer.Length;
            while (value != 0)
            {
                int digit = value % 10;
                pos = pos - 1;
                buffer[pos] = (char)('0' + digit);
                value = value / 10;
            }
            System.Text.StringBuilder result = new System.Text.StringBuilder();
            for (int i = pos; i < buffer.Length; i++) result.Append(buffer[i]);
            return result.ToString();
        }

        public string ToString(string format) { return NumberFormatter.Format(this, 4, format); }
        public string ToString(IFormatProvider provider) { NumberFormatter.Ask(false, null, provider); return ToString(); }
        public string ToString(string format, IFormatProvider provider) { NumberFormatter.Ask(false, format, provider); return NumberFormatter.Format(this, 4, format); }

        public bool Equals(ushort obj) { return this == obj; }

        public override bool Equals(object obj)
        {
            if (obj is ushort) return this == (ushort)obj;
            return false;
        }

        public override int GetHashCode() { return this; }

        public int CompareTo(ushort value)
        {
            if (this < value) return -1;
            if (this > value) return 1;
            return 0;
        }

        public int CompareTo(object obj)
        {
            if (obj == null) return 1;
            if (!(obj is ushort)) throw new ArgumentException("Object must be of type UInt16.");
            return CompareTo((ushort)obj);
        }

        public static ushort Parse(string s)
        {
            if ((object)s == null) throw new ArgumentNullException("s");
            ulong bits;
            int outcome = NumberParser.ScanWholeNumber(s, (int)NumberStyles.Integer, 4, 65535UL, 0UL, out bits);
            if (outcome != NumberParser.Done) NumberParser.Refuse(outcome, s, "Value was either too large or too small for a UInt16.");
            return (ushort)bits;
        }

        public static ushort Parse(string s, NumberStyles style)
        {
            if ((object)s == null) throw new ArgumentNullException("s");
            NumberParser.CheckWholeNumberStyle(style);
            ulong bits;
            int outcome = NumberParser.ScanWholeNumber(s, (int)style, 4, 65535UL, 0UL, out bits);
            if (outcome != NumberParser.Done) NumberParser.Refuse(outcome, s, "Value was either too large or too small for a UInt16.");
            return (ushort)bits;
        }

        public static ushort Parse(string s, IFormatProvider provider)
        {
            return Parse(s, NumberStyles.Integer, provider);
        }

        public static ushort Parse(string s, NumberStyles style, IFormatProvider provider)
        {
            if ((object)s == null) throw new ArgumentNullException("s");
            NumberParser.CheckWholeNumberStyle(style);
            NumberParser.AskProvider(provider);
            ulong bits;
            int outcome = NumberParser.ScanWholeNumber(s, (int)style, 4, 65535UL, 0UL, out bits);
            if (outcome != NumberParser.Done) NumberParser.Refuse(outcome, s, "Value was either too large or too small for a UInt16.");
            return (ushort)bits;
        }

#if LAMELLA_SURFACE_NETFX_2_0
        public static bool TryParse(string s, out ushort result)
        {
            result = 0;
            if ((object)s == null) return false;
            ulong bits;
            if (NumberParser.ScanWholeNumber(s, (int)NumberStyles.Integer, 4, 65535UL, 0UL, out bits) != NumberParser.Done) return false;
            result = (ushort)bits;
            return true;
        }

        public static bool TryParse(string s, NumberStyles style, IFormatProvider provider, out ushort result)
        {
            NumberParser.CheckWholeNumberStyle(style);
            result = 0;
            if ((object)s == null) return false;
            NumberParser.AskProvider(provider);
            ulong bits;
            if (NumberParser.ScanWholeNumber(s, (int)style, 4, 65535UL, 0UL, out bits) != NumberParser.Done) return false;
            result = (ushort)bits;
            return true;
        }
#endif
    }
}
