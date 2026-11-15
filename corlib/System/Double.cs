// Lamella managed corlib (from scratch). -- System.Double
#if LAMELLA_SURFACE_FLOAT
using System.Globalization;

namespace System
{
    public struct Double : IComparable, IFormattable
    {
        public const double MaxValue = 1.7976931348623157E+308;
        public const double MinValue = -1.7976931348623157E+308;
        public const double Epsilon = 4.9406564584124654E-324;
        public const double NaN = (double)0.0 / (double)0.0;
        public const double PositiveInfinity = (double)1.0 / (double)0.0;
        public const double NegativeInfinity = (double)(-1.0) / (double)0.0;

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
            if (!(obj is double)) throw new ArgumentException("Object must be of type Double.");
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

        public string ToString(string format)
        {
            return Format(this, format);
        }

        public string ToString(IFormatProvider provider)
        {
            NumberParser.AskProvider(provider);
            return ToString();
        }

        public string ToString(string format, IFormatProvider provider)
        {
            NumberParser.AskProvider(provider);
            return Format(this, format);
        }

        private static string Format(double value, string format)
        {
            if (!IsFinite(value)) return value.ToString();
            int precision;
            char specifier = NumberFormatter.Specifier(format, out precision);
            if (specifier == '\0') return NumberFormatter.CustomFromExponential(format, ToExponential(value, 14, true));
            if (specifier == 'R' || specifier == 'r') precision = 0;
            if (specifier == 'G' || specifier == 'g' || specifier == 'R' || specifier == 'r')
            {
                char general = (specifier == 'G' || specifier == 'R') ? 'G' : 'g';
                if (precision < 1)
                {
                    string shortest = value.ToString();
                    return general == 'G' ? shortest : shortest.Replace('E', 'e');
                }
                return NumberFormatter.GeneralFromExponential(ToExponential(value, precision - 1, true), precision, general);
            }
            if (specifier == 'E' || specifier == 'e') return ToExponential(value, precision < 0 ? 6 : precision, specifier == 'E');
            if (specifier == 'F' || specifier == 'f') return ToFixed(value, precision < 0 ? 2 : precision);
            if (specifier == 'N' || specifier == 'n' || specifier == 'C' || specifier == 'c')
            {
                int places = precision < 0 ? 2 : precision;
                return NumberFormatter.FromFixed(specifier, places, ToFixed(value, places));
            }
            if (specifier == 'P' || specifier == 'p')
            {
                int places = precision < 0 ? 2 : precision;
                return NumberFormatter.FromFixed(specifier, places, ToFixed(value, places + 2));
            }
            throw new FormatException("Format specifier was invalid.");
        }

        private static bool IsFinite(double value)
        {
            return (value == value) && ((value - value) == 0.0);
        }

        private static double FromText(string text)
        {
            if (text.Length == 3 && text[0] == 'N') return NaN;
            return ParseValid(text);
        }

        public static double Parse(string s)
        {
            if ((object)s == null) throw new ArgumentNullException("s");
            string text = NumberParser.FloatingPointText(s, (int)(NumberStyles.Float | NumberStyles.AllowThousands));
            if ((object)text == null) NumberParser.Refuse(NumberParser.Malformed, s, null);
            return FromText(text);
        }

        public static double Parse(string s, NumberStyles style)
        {
            if ((object)s == null) throw new ArgumentNullException("s");
            NumberParser.CheckFractionalStyle(style);
            string text = NumberParser.FloatingPointText(s, (int)style);
            if ((object)text == null) NumberParser.Refuse(NumberParser.Malformed, s, null);
            return FromText(text);
        }

        public static double Parse(string s, IFormatProvider provider)
        {
            return Parse(s, NumberStyles.Float | NumberStyles.AllowThousands, provider);
        }

        public static double Parse(string s, NumberStyles style, IFormatProvider provider)
        {
            if ((object)s == null) throw new ArgumentNullException("s");
            NumberParser.CheckFractionalStyle(style);
            NumberParser.AskProvider(provider);
            string text = NumberParser.FloatingPointText(s, (int)style);
            if ((object)text == null) NumberParser.Refuse(NumberParser.Malformed, s, null);
            return FromText(text);
        }

#if LAMELLA_SURFACE_NETFX_2_0
        public static bool TryParse(string s, out double result)
        {
            result = 0;
            if ((object)s == null) return false;
            string text = NumberParser.FloatingPointText(s, (int)(NumberStyles.Float | NumberStyles.AllowThousands));
            if ((object)text == null) return false;
            result = FromText(text);
            return true;
        }

        public static bool TryParse(string s, NumberStyles style, IFormatProvider provider, out double result)
        {
            NumberParser.CheckFractionalStyle(style);
            result = 0;
            if ((object)s == null) return false;
            NumberParser.AskProvider(provider);
            string text = NumberParser.FloatingPointText(s, (int)style);
            if ((object)text == null) return false;
            result = FromText(text);
            return true;
        }
#endif
    }
}
#endif
