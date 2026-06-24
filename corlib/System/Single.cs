// Lamella managed corlib (from scratch). -- System.Single
namespace System
{
    public struct Single : IComparable, IFormattable
    {
        public const float MinValue = -3.40282347E+38f;
        public const float MaxValue = 3.40282347E+38f;
        public const float Epsilon = 1.401298E-45f;
        public static readonly float NaN = BitConverter.Int32BitsToSingle(0x7FC00000);
        public static readonly float PositiveInfinity = BitConverter.Int32BitsToSingle(0x7F800000);
        public static readonly float NegativeInfinity = BitConverter.Int32BitsToSingle(unchecked((int)0xFF800000));

        public static bool IsNaN(float f)
        {
            return f != f;
        }

        public static bool IsInfinity(float f)
        {
            return f == PositiveInfinity || f == NegativeInfinity;
        }

        public static bool IsPositiveInfinity(float f)
        {
            return f == PositiveInfinity;
        }

        public static bool IsNegativeInfinity(float f)
        {
            return f == NegativeInfinity;
        }

        public int CompareTo(object obj)
        {
            if (obj == null) return 1;
            return CompareTo((float)obj);
        }

        public int CompareTo(float value)
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
            if (obj is float)
            {
                float other = (float)obj;
                if (this != this && other != other) return true;
                return this == other;
            }
            return false;
        }

        public bool Equals(float obj)
        {
            if (this != this && obj != obj) return true;
            return this == obj;
        }

        public override int GetHashCode()
        {
            return BitConverter.SingleToInt32Bits(this);
        }

        [Lamella.Runtime.RuntimeProvided] public override string ToString() { return null; }

        [Lamella.Runtime.RuntimeProvided] private static string ToFixed(float value, int decimals) { return null; }

        [Lamella.Runtime.RuntimeProvided] private static string ToExponential(float value, int precision, bool upper) { return null; }

        [Lamella.Runtime.RuntimeProvided] private static float ParseValid(string s) { return 0; }

        public static float Parse(string s)
        {
            if ((object)s == null) throw new ArgumentNullException("s");
            if (!ParseValidate(s)) throw new FormatException("Input string was not in a correct format.");
            return ParseValid(s);
        }

        public static bool TryParse(string s, out float result)
        {
            result = 0f;
            if ((object)s == null || !ParseValidate(s)) return false;
            result = ParseValid(s);
            return true;
        }

        private static bool ParseValidate(string s)
        {
            int end = s.Length;
            while (end > 0 && Char.IsWhiteSpace(s[end - 1])) end = end - 1;
            int i = 0;
            while (i < end && Char.IsWhiteSpace(s[i])) i = i + 1;
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

        private static string Format(float value, string format)
        {
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
            if (specifier == 'C' || specifier == 'c') return System.Double.Currency(value, precision < 0 ? 2 : precision);
            if (specifier == 'P' || specifier == 'p') return System.Double.Percent(value, precision < 0 ? 2 : precision);
            throw new FormatException("Format specifier was invalid.");
        }

        private static bool IsLetter(char c)
        {
            return (c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z');
        }

        private static string Grouped(float value, int decimals)
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
