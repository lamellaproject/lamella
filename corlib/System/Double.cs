// Lamella managed corlib (from scratch). -- System.Double
namespace System
{
    public struct Double : IComparable, IFormattable
    {
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

        [Lamella.Runtime.RuntimeProvided] private static string ToFixed(double value, int decimals) { return null; }

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
            throw new FormatException("Format specifier was invalid.");
        }

        private static bool IsLetter(char c)
        {
            return (c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z');
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
