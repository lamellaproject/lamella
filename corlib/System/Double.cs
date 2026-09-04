// Lamella managed corlib (from scratch). -- System.Double
#if LAMELLA_SURFACE_FLOAT
namespace System
{
    public struct Double : IComparable, IFormattable
    {
        public static readonly double MaxValue = BitConverter.Int64BitsToDouble(0x7FEFFFFFFFFFFFFFL);
        public static readonly double MinValue = BitConverter.Int64BitsToDouble(unchecked((long)0xFFEFFFFFFFFFFFFF));
        public static readonly double Epsilon = BitConverter.Int64BitsToDouble(0x0000000000000001L);
        public static readonly double NaN = BitConverter.Int64BitsToDouble(0x7FF8000000000000L);
        public static readonly double PositiveInfinity = BitConverter.Int64BitsToDouble(0x7FF0000000000000L);
        public static readonly double NegativeInfinity = BitConverter.Int64BitsToDouble(unchecked((long)0xFFF0000000000000));

        public static bool IsNaN(double d)
        {
            return d != d;
        }

        public static bool IsInfinity(double d)
        {
            return d == PositiveInfinity || d == NegativeInfinity;
        }

        public static bool IsPositiveInfinity(double d)
        {
            return d == PositiveInfinity;
        }

        public static bool IsNegativeInfinity(double d)
        {
            return d == NegativeInfinity;
        }

        public int CompareTo(object obj)
        {
            if (obj == null) return 1;
            return CompareTo((double)obj);
        }

        public int CompareTo(double value)
        {
            if (this < value) return -1;
            if (this > value) return 1;
            if (this == value) return 0;
            bool thisNaN = this != this;
            bool otherNaN = value != value;
            if (thisNaN && otherNaN) return 0;
            if (thisNaN) return -1;
            return 1;
        }

        public override bool Equals(object obj)
        {
            if (obj is double)
            {
                double other = (double)obj;
                if (this != this && other != other) return true;
                return this == other;
            }
            return false;
        }

        public bool Equals(double obj)
        {
            if (this != this && obj != obj) return true;
            return this == obj;
        }

        public override int GetHashCode()
        {
            long bits = BitConverter.DoubleToInt64Bits(this);
            return ((int)bits) ^ ((int)(bits >> 32));
        }

        [Lamella.Runtime.RuntimeProvided] public override string ToString() { return null; }

        [Lamella.Runtime.RuntimeProvided] internal static string ToFixed(double value, int decimals) { return null; }

        [Lamella.Runtime.RuntimeProvided] internal static string ToExponential(double value, int precision, bool upper) { return null; }

        [Lamella.Runtime.RuntimeProvided] private static double ParseValid(string s) { return 0; }

        public static double Parse(string s)
        {
            if ((object)s == null) throw new ArgumentNullException("s");
            if (!ParseValidate(s)) throw new FormatException("Input string was not in a correct format.");
            return ParseValid(s);
        }

#if LAMELLA_SURFACE_NETFX_2_0
        public static bool TryParse(string s, out double result)
        {
            result = 0.0;
            if ((object)s == null || !ParseValidate(s)) return false;
            result = ParseValid(s);
            return true;
        }
#endif

        private static bool ParseValidate(string s)
        {
            int end = s.Length;
            while (end > 0 && NumberText.IsPad(s[end - 1])) end = end - 1;
            int i = 0;
            while (i < end && NumberText.IsPad(s[i])) i = i + 1;
            if (i >= end) return false;

            string core = s.Substring(i, end - i);
            if (EqualsIgnoreCase(core, "NaN")) return true;
            if (EqualsIgnoreCase(core, "Infinity") || EqualsIgnoreCase(core, "+Infinity")
                || EqualsIgnoreCase(core, "-Infinity")) return true;

            if (s[i] == '+' || s[i] == '-') i = i + 1;
            bool sawDigit = false;
            bool sawDot = false;
            while (i < end)
            {
                char c = s[i];
                if (c >= '0' && c <= '9') { sawDigit = true; i = i + 1; continue; }
                if (c == '.') { if (sawDot) return false; sawDot = true; i = i + 1; continue; }
                if (c == ',') { if (sawDot || !sawDigit) return false; i = i + 1; continue; }
                if (c == 'e' || c == 'E') break;
                return false;
            }
            if (!sawDigit) return false;
            if (i >= end) return true;

            i = i + 1;
            if (i < end && (s[i] == '+' || s[i] == '-')) i = i + 1;
            bool sawExpDigit = false;
            while (i < end)
            {
                char c = s[i];
                if (c < '0' || c > '9') return false;
                sawExpDigit = true;
                i = i + 1;
            }
            return sawExpDigit;
        }

        private static bool EqualsIgnoreCase(string a, string b)
        {
            if (a.Length != b.Length) return false;
            for (int k = 0; k < a.Length; k++)
            {
                char ca = a[k]; char cb = b[k];
                if (ca >= 'A' && ca <= 'Z') ca = (char)(ca + 32);
                if (cb >= 'A' && cb <= 'Z') cb = (char)(cb + 32);
                if (ca != cb) return false;
            }
            return true;
        }

        public string ToString(string format)
        {
            return Format(this, format);
        }

        public string ToString(string format, IFormatProvider formatProvider)
        {
            return Format(this, format);
        }

        private static string Format(double value, string format)
        {
            if (NumberFormatter.IsCustom(format))
            {
                if (!IsFinite(value)) return value.ToString();
                bool negativeValue = value < 0;
                return NumberFormatter.CustomFloat(format, negativeValue, negativeValue ? -value : value);
            }
            char specifier = 'G';
            int precision = -1;
            if ((object)format != null && format.Length != 0)
            {
                specifier = format[0];
                if (!IsLetter(specifier)) throw new FormatException("Format specifier was invalid.");
                int i = 1;
                int p = 0;
                bool sawDigit = false;
                while (i < format.Length)
                {
                    char c = format[i];
                    if (c < '0' || c > '9') throw new FormatException("Format specifier was invalid.");
                    sawDigit = true;
                    if (p < 1000000) p = p * 10 + (c - '0');
                    i++;
                }
                if (sawDigit) precision = p;
            }

            if (specifier == 'G' || specifier == 'g') return value.ToString();
            if (specifier == 'F' || specifier == 'f') return ToFixed(value, precision < 0 ? 2 : precision);
            if (specifier == 'N' || specifier == 'n') return Grouped(value, precision < 0 ? 2 : precision);
            if (specifier == 'E' || specifier == 'e') return ToExponential(value, precision < 0 ? 6 : precision, specifier == 'E');
            if (specifier == 'C' || specifier == 'c') return Currency(value, precision < 0 ? 2 : precision);
            if (specifier == 'P' || specifier == 'p') return Percent(value, precision < 0 ? 2 : precision);
            throw new FormatException("Format specifier was invalid.");
        }

        private static bool IsLetter(char c)
        {
            return (c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z');
        }

        internal static string Currency(double value, int decimals)
        {
            if (!IsFinite(value)) return ToFixed(value, decimals);
            bool negative = value < 0;
            double magnitude = negative ? -value : value;
            string body = ((char)0x00A4).ToString() + Grouped(magnitude, decimals);
            return negative ? "(" + body + ")" : body;
        }

        internal static string Percent(double value, int decimals)
        {
            if (!IsFinite(value)) return ToFixed(value, decimals);
            return Grouped(value * 100.0, decimals) + " %";
        }

        private static bool IsFinite(double value)
        {
            return (value == value) && ((value - value) == 0.0);
        }

        private static string Grouped(double value, int decimals)
        {
            string fixedText = ToFixed(value, decimals);
            int probe = 0;
            if (probe < fixedText.Length && fixedText[probe] == '-') probe++;
            if (probe >= fixedText.Length || fixedText[probe] < '0' || fixedText[probe] > '9') return fixedText;

            System.Text.StringBuilder result = new System.Text.StringBuilder();
            int start = 0;
            if (fixedText[0] == '-') { result.Append('-'); start = 1; }
            int dot = fixedText.Length;
            for (int k = start; k < fixedText.Length; k++)
            {
                if (fixedText[k] == '.') { dot = k; break; }
            }
            int intDigits = dot - start;
            for (int k = 0; k < intDigits; k++)
            {
                if (k != 0 && (intDigits - k) % 3 == 0) result.Append(',');
                result.Append(fixedText[start + k]);
            }
            for (int k = dot; k < fixedText.Length; k++) result.Append(fixedText[k]);
            return result.ToString();
        }
    }
}
#endif
