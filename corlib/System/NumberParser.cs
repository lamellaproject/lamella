// Lamella managed corlib (from scratch). -- System.NumberParser (internal)
using System.Globalization;

namespace System
{
    internal sealed class NumberParser
    {
        private NumberParser() { }

        internal const int Done = 1;
        internal const int Malformed = 0;
        internal const int TooLarge = -2;

        private const int AllowLeadingWhite = 0x001;
        private const int AllowTrailingWhite = 0x002;
        private const int AllowLeadingSign = 0x004;
        private const int AllowTrailingSign = 0x008;
        private const int AllowParentheses = 0x010;
        private const int AllowDecimalPoint = 0x020;
        private const int AllowThousands = 0x040;
        private const int AllowExponent = 0x080;
        private const int AllowCurrencySymbol = 0x100;
        private const int AllowHexSpecifier = 0x200;
        private const int HexNumber = AllowLeadingWhite | AllowTrailingWhite | AllowHexSpecifier;
        private const int DefinedStyles = 0x3FF;

        private const int WholeNumber = 0;
        private const int DecimalNumber = 1;
        private const int FloatingPoint = 2;

        private const int FloatingPointDigits = 768 + 1;
        private const int DecimalDigits = 30 + 1;

        private const int SawSign = 0x01;
        private const int InParentheses = 0x02;
        private const int SawDigit = 0x04;
        private const int SawNonZero = 0x08;
        private const int SawPoint = 0x10;
        private const int SawCurrency = 0x20;

        private const char CurrencySymbol = '¤';

        internal static void CheckWholeNumberStyle(NumberStyles style)
        {
            int styles = (int)style;
            if ((styles & ~DefinedStyles) != 0)
                throw new ArgumentException("An undefined NumberStyles value is being used.", "style");
            if ((styles & AllowHexSpecifier) != 0 && (styles & ~HexNumber) != 0)
                throw new ArgumentException("With the AllowHexSpecifier or AllowBinarySpecifier bit set in the enum bit field, the only other valid bits that can be combined into the enum value must be AllowLeadingWhite and AllowTrailingWhite.", "style");
        }

#if LAMELLA_SURFACE_FLOAT || LAMELLA_SURFACE_DECIMAL
        internal static void CheckFractionalStyle(NumberStyles style)
        {
            int styles = (int)style;
            if ((styles & ~DefinedStyles) != 0)
                throw new ArgumentException("An undefined NumberStyles value is being used.", "style");
            if ((styles & AllowHexSpecifier) != 0)
                throw new ArgumentException("The number styles AllowHexSpecifier and AllowBinarySpecifier are not supported on floating point data types.", "style");
        }
#endif

        internal static void AskProvider(IFormatProvider provider)
        {
            if ((object)provider == null) return;
            if (provider is NumberFormatInfo) return;
            // CultureInfo subclasses can override GetFormat to observe or refuse the request.
            provider.GetFormat(typeof(NumberFormatInfo));
        }

        internal static void Refuse(int outcome, string s, string tooLarge)
        {
            if (outcome == TooLarge) throw new OverflowException(tooLarge);
            throw new FormatException("The input string '" + s + "' was not in a correct format.");
        }

        internal static int ScanWholeNumber(string s, int styles, int hexDigits, ulong positive, ulong negative, out ulong bits)
        {
            bits = 0;
            if ((styles & AllowHexSpecifier) != 0) return ScanHex(s, styles, hexDigits, out bits);
            int count;
            int scale;
            bool isNegative;
            if (!Scan(s, styles, WholeNumber, null, out count, out scale, out isNegative)) return Malformed;
            if (scale > 20 || scale < count) return TooLarge;
            ulong magnitude = 0;
            int at = 0;
            for (int i = 0; i < scale; i = i + 1)
            {
                if (magnitude > 1844674407370955161UL) return TooLarge;
                magnitude = magnitude * 10;
                if (i < count)
                {
                    char c = s[at];
                    while (c < (i == 0 ? '1' : '0') || c > '9')
                    {
                        at = at + 1;
                        c = s[at];
                    }
                    at = at + 1;
                    ulong next = magnitude + (ulong)(c - '0');
                    if (next < magnitude) return TooLarge;
                    magnitude = next;
                }
            }
            if (isNegative)
            {
                if (negative == 0 || magnitude > negative) return TooLarge;
                bits = 0 - magnitude;
                return Done;
            }
            if (magnitude > positive) return TooLarge;
            bits = magnitude;
            return Done;
        }

        private static int ScanHex(string s, int styles, int hexDigits, out ulong bits)
        {
            bits = 0;
            int length = s.Length;
            int i = 0;
            if ((styles & AllowLeadingWhite) != 0)
            {
                while (i < length && NumberText.IsPad(s[i])) i = i + 1;
            }
            if (i >= length || HexValue(s[i]) < 0) return Malformed;
            while (i < length && s[i] == '0') i = i + 1;
            ulong value = 0;
            int taken = 0;
            while (i < length)
            {
                int digit = HexValue(s[i]);
                if (digit < 0) break;
                value = (value << 4) | (ulong)digit;
                taken = taken + 1;
                i = i + 1;
            }
            if ((styles & AllowTrailingWhite) != 0)
            {
                while (i < length && NumberText.IsPad(s[i])) i = i + 1;
            }
            while (i < length)
            {
                if (s[i] != '\0') return Malformed;
                i = i + 1;
            }
            if (taken > hexDigits) return TooLarge;
            bits = value;
            return Done;
        }

        private static int HexValue(char c)
        {
            if (c >= '0' && c <= '9') return c - '0';
            if (c >= 'a' && c <= 'f') return c - 'a' + 10;
            if (c >= 'A' && c <= 'F') return c - 'A' + 10;
            return -1;
        }

        private static bool Scan(string s, int styles, int kind, char[] digits, out int count, out int scale, out bool negative)
        {
            count = 0;
            scale = 0;
            negative = false;
            int length = s.Length;
            int state = 0;
            bool currencyAllowed = (styles & AllowCurrencySymbol) != 0;
            int i = 0;
            char c = i < length ? s[i] : '\0';
            while (true)
            {
                if (NumberText.IsPad(c) && (styles & AllowLeadingWhite) != 0
                    && ((state & SawSign) == 0 || (state & SawCurrency) != 0))
                {
                }
                else if ((styles & AllowLeadingSign) != 0 && (state & SawSign) == 0 && (c == '+' || c == '-'))
                {
                    state = state | SawSign;
                    if (c == '-') negative = true;
                }
                else if (c == '(' && (styles & AllowParentheses) != 0 && (state & SawSign) == 0)
                {
                    state = state | SawSign | InParentheses;
                    negative = true;
                }
                else if (currencyAllowed && c == CurrencySymbol)
                {
                    state = state | SawCurrency;
                    currencyAllowed = false;
                }
                else
                {
                    break;
                }
                i = i + 1;
                c = i < length ? s[i] : '\0';
            }

            int stored = 0;
            int end = 0;
            while (true)
            {
                if (c >= '0' && c <= '9')
                {
                    state = state | SawDigit;
                    if (c != '0' || (state & SawNonZero) != 0)
                    {
                        if ((object)digits == null || stored < digits.Length - 1)
                        {
                            if ((object)digits != null) digits[stored] = c;
                            stored = stored + 1;
                            if (c != '0' || kind != WholeNumber) end = stored;
                        }
                        else if (c != '0')
                        {
                            digits[stored] = c;
                            end = stored + 1;
                        }
                        if ((state & SawPoint) == 0) scale = scale + 1;
                        state = state | SawNonZero;
                    }
                    else if ((state & SawPoint) != 0)
                    {
                        scale = scale - 1;
                    }
                }
                else if (c == '.' && (styles & AllowDecimalPoint) != 0 && (state & SawPoint) == 0)
                {
                    state = state | SawPoint;
                }
                else if (c == ',' && (styles & AllowThousands) != 0 && (state & SawDigit) != 0 && (state & SawPoint) == 0)
                {
                }
                else
                {
                    break;
                }
                i = i + 1;
                c = i < length ? s[i] : '\0';
            }
            count = end;
            if ((state & SawDigit) == 0) return false;

            if ((c == 'e' || c == 'E') && (styles & AllowExponent) != 0)
            {
                int mark = i;
                i = i + 1;
                c = i < length ? s[i] : '\0';
                bool exponentNegative = false;
                if (c == '+' || c == '-')
                {
                    exponentNegative = c == '-';
                    i = i + 1;
                    c = i < length ? s[i] : '\0';
                }
                if (c >= '0' && c <= '9')
                {
                    int exponent = 0;
                    while (c >= '0' && c <= '9')
                    {
                        if (exponent >= 100000000)
                        {
                            exponent = Int32.MaxValue;
                            scale = 0;
                            while (c >= '0' && c <= '9')
                            {
                                i = i + 1;
                                c = i < length ? s[i] : '\0';
                            }
                            break;
                        }
                        exponent = exponent * 10 + (c - '0');
                        i = i + 1;
                        c = i < length ? s[i] : '\0';
                    }
                    scale = scale + (exponentNegative ? -exponent : exponent);
                }
                else
                {
                    i = mark;
                    c = s[i];
                }
            }

            while (true)
            {
                if (NumberText.IsPad(c) && (styles & AllowTrailingWhite) != 0)
                {
                }
                else if ((styles & AllowTrailingSign) != 0 && (state & SawSign) == 0 && (c == '+' || c == '-'))
                {
                    state = state | SawSign;
                    if (c == '-') negative = true;
                }
                else if (c == ')' && (state & InParentheses) != 0)
                {
                    state = state & ~InParentheses;
                }
                else if (currencyAllowed && c == CurrencySymbol)
                {
                    currencyAllowed = false;
                }
                else
                {
                    break;
                }
                i = i + 1;
                c = i < length ? s[i] : '\0';
            }
            if ((state & InParentheses) != 0) return false;
            if ((state & SawNonZero) == 0)
            {
                if (kind != DecimalNumber) scale = 0;
                if (kind == WholeNumber && (state & SawPoint) == 0) negative = false;
            }
            while (i < length)
            {
                if (s[i] != '\0') return false;
                i = i + 1;
            }
            return true;
        }

#if LAMELLA_SURFACE_FLOAT
        internal static string FloatingPointText(string s, int styles)
        {
            char[] digits = new char[s.Length < FloatingPointDigits ? s.Length + 1 : FloatingPointDigits];
            int count;
            int scale;
            bool negative;
            if (!Scan(s, styles, FloatingPoint, digits, out count, out scale, out negative)) return NamedValue(s);
            string sign = negative ? "-" : "";
            if (count == 0) return sign + "0";
            return sign + new String(digits, 0, count) + "e" + ((long)scale - count).ToString();
        }

        private static string NamedValue(string s)
        {
            int start = 0;
            int end = s.Length;
            while (start < end && IsTrimmed(s[start])) start = start + 1;
            while (end > start && IsTrimmed(s[end - 1])) end = end - 1;
            if (Names(s, start, end, "Infinity")) return "Infinity";
            if (Names(s, start, end, "NaN")) return "NaN";
            if (start < end && s[start] == '+')
            {
                if (Names(s, start + 1, end, "Infinity")) return "Infinity";
                if (Names(s, start + 1, end, "NaN")) return "NaN";
            }
            if (start < end && s[start] == '-')
            {
                if (Names(s, start + 1, end, "Infinity")) return "-Infinity";
                if (Names(s, start + 1, end, "NaN")) return "NaN";
            }
            return null;
        }

        private static bool IsTrimmed(char c)
        {
            if (c < (char)0x80) return (c >= (char)0x09 && c <= (char)0x0D) || c == (char)0x20;
            return c == (char)0x85 || c == (char)0xA0 || c == (char)0x1680 || (c >= (char)0x2000 && c <= (char)0x200A)
                || c == (char)0x2028 || c == (char)0x2029 || c == (char)0x202F || c == (char)0x205F || c == (char)0x3000;
        }

        private static bool Names(string s, int start, int end, string name)
        {
            if (end - start != name.Length) return false;
            for (int k = 0; k < name.Length; k = k + 1)
            {
                char a = s[start + k];
                char b = name[k];
                if (a >= 'A' && a <= 'Z') a = (char)(a + 32);
                if (b >= 'A' && b <= 'Z') b = (char)(b + 32);
                if (a != b) return false;
            }
            return true;
        }
#endif

#if LAMELLA_SURFACE_DECIMAL
        internal static int ScanDecimal(string s, int styles, out Decimal result)
        {
            result = Decimal.Zero;
            char[] digits = new char[s.Length < DecimalDigits ? s.Length + 1 : DecimalDigits];
            int count;
            int scale;
            bool negative;
            if (!Scan(s, styles, DecimalNumber, digits, out count, out scale, out negative)) return Malformed;
            if (count == 0)
            {
                int zeroScale = -scale;
                if (zeroScale < 0) zeroScale = 0;
                if (zeroScale > 28) zeroScale = 28;
                result = new Decimal(0, 0, 0, negative, (byte)zeroScale);
                return Done;
            }
            if (scale > 29) return TooLarge;
            uint low = 0;
            uint middle = 0;
            uint high = 0;
            int next = 0;
            while (scale > 0 || (next < count && scale > -28))
            {
                int digit = next < count ? digits[next] - '0' : 0;
                if (high > 0x19999999u) break;
                if (high == 0x19999999u)
                {
                    if (middle > 0x99999999u) break;
                    if (middle == 0x99999999u)
                    {
                        if (low > 0x99999999u) break;
                        if (low == 0x99999999u && digit > 5) break;
                    }
                }
                ulong carry = (ulong)low * 10 + (ulong)digit;
                low = (uint)carry;
                carry = (ulong)middle * 10 + (carry >> 32);
                middle = (uint)carry;
                carry = (ulong)high * 10 + (carry >> 32);
                high = (uint)carry;
                if (next < count) next = next + 1;
                scale = scale - 1;
            }
            if (next < count && digits[next] >= '5')
            {
                bool roundUp = digits[next] != '5' || (low & 1) != 0;
                for (int k = next + 1; k < count && !roundUp; k = k + 1)
                {
                    if (digits[k] != '0') roundUp = true;
                }
                if (roundUp)
                {
                    low = low + 1;
                    if (low == 0)
                    {
                        middle = middle + 1;
                        if (middle == 0)
                        {
                            high = high + 1;
                            if (high == 0)
                            {
                                low = 0x9999999Au;
                                middle = 0x99999999u;
                                high = 0x19999999u;
                                scale = scale + 1;
                            }
                        }
                    }
                }
            }
            if (scale > 0) return TooLarge;
            if (scale <= -29)
            {
                result = new Decimal(0, 0, 0, negative, 28);
                return Done;
            }
            result = new Decimal((int)low, (int)middle, (int)high, negative, (byte)(-scale));
            return Done;
        }
#endif
    }
}
