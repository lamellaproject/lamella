// Lamella managed corlib (from scratch). -- System.NumberFormatter (internal)
namespace System
{
    internal sealed class NumberFormatter
    {
        internal static string Format(long value, int nibbles, string format)
        {
            bool negative = value < 0;
            ulong magnitude = negative ? (ulong)(-(value + 1)) + 1 : (ulong)value;
            return Whole(negative, magnitude, value, nibbles, format);
        }

        internal static string FormatUnsigned(ulong value, string format)
        {
            return Whole(false, value, (long)value, 16, format);
        }

        internal static char Specifier(string format, out int precision)
        {
            precision = -1;
            if ((object)format == null || format.Length == 0) return 'G';
            char c = format[0];
            if (c == '\0') return 'G';
            if (!IsLetter(c)) return '\0';
            int n = 0;
            int i = 1;
            while (i < format.Length && format[i] >= '0' && format[i] <= '9')
            {
                if (n >= 100000000) throw new FormatException("Format specifier was invalid.");
                n = n * 10 + (format[i] - '0');
                i = i + 1;
            }
            if (i < format.Length && format[i] != '\0') return '\0';
            if (format.Length > 1) precision = n;
            return c;
        }

        internal static void Ask(bool negative, string format, IFormatProvider provider)
        {
            if ((object)provider == null) return;
            int precision;
            char specifier = Specifier(format, out precision);
            if (specifier == 'X' || specifier == 'x' || specifier == 'B' || specifier == 'b') return;
            bool plain = specifier == 'D' || specifier == 'd' || ((specifier == 'G' || specifier == 'g') && precision < 1);
            if (plain && !negative) return;
            NumberParser.AskProvider(provider);
        }

        private static string Whole(bool negative, ulong magnitude, long bits, int nibbles, string format)
        {
            int precision;
            char specifier = Specifier(format, out precision);
            if (specifier == '\0')
            {
                string text = DigitsOf(magnitude);
                return CustomNumberFormat.Render(format, negative, text, text.Length, false);
            }
            if (specifier == 'X') return Hex(bits, nibbles, precision, true);
            if (specifier == 'x') return Hex(bits, nibbles, precision, false);
            if (specifier == 'B' || specifier == 'b') return Binary(bits, nibbles, precision);
            string digits = DigitsOf(magnitude);
            if (specifier == 'D' || specifier == 'd') return Plain(negative, digits, precision);
            if (specifier == 'R') specifier = 'G';
            if (specifier == 'r') specifier = 'g';
            if ((specifier == 'G' || specifier == 'g') && precision < 1) return Plain(negative, digits, 0);
            return Standard(specifier, precision, negative, digits, digits.Length, digits.Length, false);
        }

        private static string DigitsOf(ulong magnitude)
        {
            if (magnitude == 0) return "";
            char[] buffer = new char[20];
            int pos = buffer.Length;
            while (magnitude != 0)
            {
                pos = pos - 1;
                buffer[pos] = (char)('0' + (int)(magnitude % 10));
                magnitude = magnitude / 10;
            }
            return new System.Text.StringBuilder().Append(buffer, pos, buffer.Length - pos).ToString();
        }

        private static string Plain(bool negative, string digits, int minDigits)
        {
            System.Text.StringBuilder result = new System.Text.StringBuilder();
            if (negative) result.Append('-');
            int count = digits.Length == 0 ? 1 : digits.Length;
            for (int i = count; i < minDigits; i++) result.Append('0');
            result.Append(digits.Length == 0 ? "0" : digits);
            return result.ToString();
        }

        private static string Standard(char specifier, int precision, bool negative, string digits, int scale, int general, bool floating)
        {
            System.Text.StringBuilder result = new System.Text.StringBuilder();
            if (specifier == 'F' || specifier == 'f' || specifier == 'N' || specifier == 'n')
            {
                int places = precision < 0 ? 2 : precision;
                digits = RoundDigits(digits, scale + places, ref scale);
                if (negative && (floating || digits.Length != 0)) result.Append('-');
                AppendFixed(result, digits, scale, places, specifier == 'N' || specifier == 'n');
                return result.ToString();
            }
            if (specifier == 'E' || specifier == 'e')
            {
                int shown = (precision < 0 ? 6 : precision) + 1;
                digits = RoundDigits(digits, shown, ref scale);
                if (negative) result.Append('-');
                AppendScientific(result, digits, scale, shown, specifier);
                return result.ToString();
            }
            if (specifier == 'G' || specifier == 'g')
            {
                int width = precision < 1 ? general : precision;
                digits = RoundDigits(digits, width, ref scale);
                if (negative) result.Append('-');
                AppendGeneral(result, digits, scale, width, specifier == 'G' ? 'E' : 'e');
                return result.ToString();
            }
            if (specifier == 'C' || specifier == 'c')
            {
                int places = precision < 0 ? 2 : precision;
                digits = RoundDigits(digits, scale + places, ref scale);
                if (negative && (floating || digits.Length != 0)) result.Append('(');
                result.Append((char)0x00A4);
                AppendFixed(result, digits, scale, places, true);
                if (negative && (floating || digits.Length != 0)) result.Append(')');
                return result.ToString();
            }
            if (specifier == 'P' || specifier == 'p')
            {
                int places = precision < 0 ? 2 : precision;
                scale = scale + 2;
                digits = RoundDigits(digits, scale + places, ref scale);
                if (negative && (floating || digits.Length != 0)) result.Append('-');
                AppendFixed(result, digits, scale, places, true);
                result.Append(" %");
                return result.ToString();
            }
            throw new FormatException("Format specifier was invalid.");
        }

        internal static string RoundDigits(string digits, int position, ref int scale)
        {
            int keep = position < digits.Length ? position : digits.Length;
            if (keep < 0) keep = 0;
            if (position >= 0 && position < digits.Length && digits[position] >= '5')
            {
                while (keep > 0 && digits[keep - 1] == '9') keep = keep - 1;
                if (keep == 0)
                {
                    scale = scale + 1;
                    return "1";
                }
                char[] carried = digits.Substring(0, keep).ToCharArray();
                carried[keep - 1] = (char)(carried[keep - 1] + 1);
                return new System.Text.StringBuilder().Append(carried).ToString();
            }
            while (keep > 0 && digits[keep - 1] == '0') keep = keep - 1;
            if (keep == 0) scale = 0;
            return digits.Substring(0, keep);
        }

        private static void AppendFixed(System.Text.StringBuilder result, string digits, int scale, int places, bool grouped)
        {
            int cursor = 0;
            if (scale > 0)
            {
                for (int k = 0; k < scale; k++)
                {
                    if (grouped && k != 0 && (scale - k) % 3 == 0) result.Append(',');
                    result.Append(cursor < digits.Length ? digits[cursor] : '0');
                    cursor = cursor + 1;
                }
            }
            else
            {
                result.Append('0');
            }
            if (places <= 0) return;
            result.Append('.');
            int leading = scale < 0 ? -scale : 0;
            if (leading > places) leading = places;
            result.Append('0', leading);
            for (int k = leading; k < places; k++)
            {
                result.Append(cursor < digits.Length ? digits[cursor] : '0');
                cursor = cursor + 1;
            }
        }

        private static void AppendScientific(System.Text.StringBuilder result, string digits, int scale, int shown, char letter)
        {
            result.Append(digits.Length != 0 ? digits[0] : '0');
            if (shown != 1) result.Append('.');
            for (int k = 1; k < shown; k++) result.Append(k < digits.Length ? digits[k] : '0');
            AppendExponent(result, digits.Length == 0 ? 0 : scale - 1, letter, 3);
        }

        private static void AppendGeneral(System.Text.StringBuilder result, string digits, int scale, int width, char letter)
        {
            int before = scale;
            bool scientific = before > width || before < -3;
            if (scientific) before = 1;
            int cursor = 0;
            if (before > 0)
            {
                for (int k = 0; k < before; k++)
                {
                    result.Append(cursor < digits.Length ? digits[cursor] : '0');
                    cursor = cursor + 1;
                }
            }
            else
            {
                result.Append('0');
            }
            if (cursor < digits.Length || before < 0)
            {
                result.Append('.');
                if (before < 0) result.Append('0', -before);
                for (; cursor < digits.Length; cursor++) result.Append(digits[cursor]);
            }
            if (scientific) AppendExponent(result, scale - 1, letter, 2);
        }

        private static void AppendExponent(System.Text.StringBuilder result, int exponent, char letter, int minDigits)
        {
            result.Append(letter);
            result.Append(exponent < 0 ? '-' : '+');
            result.Append(Decimal(exponent < 0 ? -exponent : exponent, minDigits));
        }

        private static bool IsLetter(char c)
        {
            return (c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z');
        }

        internal static string Decimal(long value, int minDigits)
        {
            bool negative = value < 0;
            long n = negative ? value : -value;
            char[] buffer = new char[32];
            int pos = buffer.Length;
            if (n == 0)
            {
                pos = pos - 1;
                buffer[pos] = '0';
            }
            else
            {
                while (n != 0)
                {
                    int digit = (int)(-(n % 10));
                    pos = pos - 1;
                    buffer[pos] = (char)('0' + digit);
                    n = n / 10;
                }
            }
            int digitCount = buffer.Length - pos;
            System.Text.StringBuilder result = new System.Text.StringBuilder();
            if (negative) result.Append('-');
            for (int i = digitCount; i < minDigits; i++) result.Append('0');
            for (int i = pos; i < buffer.Length; i++) result.Append(buffer[i]);
            return result.ToString();
        }

        private static string Hex(long value, int nibbles, int minDigits, bool upper)
        {
            char[] buffer = new char[nibbles];
            for (int i = 0; i < nibbles; i++)
            {
                int nibble = (int)((value >> (4 * i)) & 0xF);
                char c;
                if (nibble < 10) c = (char)('0' + nibble);
                else c = (char)((upper ? 'A' : 'a') + (nibble - 10));
                buffer[nibbles - 1 - i] = c;
            }
            int start = 0;
            while (start < nibbles - 1 && buffer[start] == '0') start++;
            int significant = nibbles - start;
            System.Text.StringBuilder result = new System.Text.StringBuilder();
            for (int i = significant; i < minDigits; i++) result.Append('0');
            for (int i = start; i < nibbles; i++) result.Append(buffer[i]);
            return result.ToString();
        }

        private static string Binary(long value, int nibbles, int minDigits)
        {
            int top = nibbles * 4 - 1;
            while (top > 0 && ((value >> top) & 1) == 0) top = top - 1;
            System.Text.StringBuilder result = new System.Text.StringBuilder();
            for (int i = top + 1; i < minDigits; i++) result.Append('0');
            for (int i = top; i >= 0; i--) result.Append(((value >> i) & 1) == 0 ? '0' : '1');
            return result.ToString();
        }

        private static void ReadExponential(string text, out bool negative, out string digits, out int scale)
        {
            negative = text[0] == '-';
            int mark = text.IndexOf('E');
            System.Text.StringBuilder taken = new System.Text.StringBuilder();
            for (int i = negative ? 1 : 0; i < mark; i++)
            {
                if (text[i] != '.') taken.Append(text[i]);
            }
            bool below = text[mark + 1] == '-';
            int exponent = 0;
            for (int i = mark + 2; i < text.Length; i++) exponent = exponent * 10 + (text[i] - '0');
            digits = taken.ToString();
            scale = (below ? -exponent : exponent) + 1;
        }

        private static void ReadPlain(string text, out bool negative, out string digits, out int scale, out int written)
        {
            negative = text[0] == '-';
            int start = negative ? 1 : 0;
            int point = text.IndexOf('.');
            int end = text.Length;
            if (point < 0) point = end;
            System.Text.StringBuilder taken = new System.Text.StringBuilder();
            scale = point - start;
            bool leading = true;
            for (int i = start; i < end; i++)
            {
                char c = text[i];
                if (c == '.') continue;
                if (leading && c == '0')
                {
                    scale = scale - 1;
                    continue;
                }
                leading = false;
                taken.Append(c);
            }
            written = taken.Length;
            int keep = written;
            while (keep > 0 && taken[keep - 1] == '0') keep = keep - 1;
            digits = taken.ToString(0, keep);
            if (keep == 0) scale = 0;
        }

        internal static string GeneralFromExponential(string text, int precision, char specifier)
        {
            bool negative;
            string digits;
            int scale;
            ReadExponential(text, out negative, out digits, out scale);
            return Standard(specifier, precision, negative, digits, scale, 0, true);
        }

        internal static string FromFixed(char specifier, int places, string text)
        {
            bool negative;
            string digits;
            int scale;
            int written;
            ReadPlain(text, out negative, out digits, out scale, out written);
            return Standard(specifier, places, negative, digits, scale, 0, true);
        }

        internal static string CustomFromExponential(string format, string text)
        {
            bool negative;
            string digits;
            int scale;
            ReadExponential(text, out negative, out digits, out scale);
            int keep = digits.Length;
            while (keep > 0 && digits[keep - 1] == '0') keep = keep - 1;
            return CustomNumberFormat.Render(format, negative, digits.Substring(0, keep), keep == 0 ? 0 : scale, true);
        }

        internal static string FormatDecimal(string text, string format)
        {
            int precision;
            char specifier = Specifier(format, out precision);
            bool negative;
            string digits;
            int scale;
            int written;
            ReadPlain(text, out negative, out digits, out scale, out written);
            if (specifier == '\0') return CustomNumberFormat.Render(format, negative, digits, scale, false);
            if (specifier == 'R') specifier = 'G';
            if (specifier == 'r') specifier = 'g';
            if ((specifier == 'G' || specifier == 'g') && precision < 0) return text;
            return Standard(specifier, precision, negative, digits, scale, written, false);
        }
    }
}
