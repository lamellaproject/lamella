// Lamella managed corlib (from scratch). -- System.Single
#if LAMELLA_SURFACE_FLOAT
using System.Globalization;

namespace System
{
    public struct Single : IComparable, IFormattable
    {
        public const float MinValue = -3.40282347E+38f;
        public const float MaxValue = 3.40282347E+38f;
        public const float Epsilon = 1.401298E-45f;
        public const float NaN = (float)0.0 / (float)0.0;
        public const float PositiveInfinity = (float)1.0 / (float)0.0;
        public const float NegativeInfinity = (float)(-1.0) / (float)0.0;

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
            if (!(obj is float)) throw new ArgumentException("Object must be of type Single.");
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

        private static string Format(float value, string format)
        {
            if (!IsFinite(value)) return value.ToString();
            int precision;
            char specifier = NumberFormatter.Specifier(format, out precision);
            if (specifier == '\0') return NumberFormatter.CustomFromExponential(format, ToExponential(value, 6, true));
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

        private static bool IsFinite(float value)
        {
            return (value == value) && ((value - value) == 0f);
        }

        private static float FromText(string text)
        {
            if (text.Length == 3 && text[0] == 'N') return NaN;
            return ParseValid(text);
        }

        public static float Parse(string s)
        {
            if ((object)s == null) throw new ArgumentNullException("s");
            string text = NumberParser.FloatingPointText(s, (int)(NumberStyles.Float | NumberStyles.AllowThousands));
            if ((object)text == null) NumberParser.Refuse(NumberParser.Malformed, s, null);
            return FromText(text);
        }

        public static float Parse(string s, NumberStyles style)
        {
            if ((object)s == null) throw new ArgumentNullException("s");
            NumberParser.CheckFractionalStyle(style);
            string text = NumberParser.FloatingPointText(s, (int)style);
            if ((object)text == null) NumberParser.Refuse(NumberParser.Malformed, s, null);
            return FromText(text);
        }

        public static float Parse(string s, IFormatProvider provider)
        {
            return Parse(s, NumberStyles.Float | NumberStyles.AllowThousands, provider);
        }

        public static float Parse(string s, NumberStyles style, IFormatProvider provider)
        {
            if ((object)s == null) throw new ArgumentNullException("s");
            NumberParser.CheckFractionalStyle(style);
            NumberParser.AskProvider(provider);
            string text = NumberParser.FloatingPointText(s, (int)style);
            if ((object)text == null) NumberParser.Refuse(NumberParser.Malformed, s, null);
            return FromText(text);
        }

#if LAMELLA_SURFACE_NETFX_2_0
        public static bool TryParse(string s, out float result)
        {
            result = 0;
            if ((object)s == null) return false;
            string text = NumberParser.FloatingPointText(s, (int)(NumberStyles.Float | NumberStyles.AllowThousands));
            if ((object)text == null) return false;
            result = FromText(text);
            return true;
        }

        public static bool TryParse(string s, NumberStyles style, IFormatProvider provider, out float result)
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
