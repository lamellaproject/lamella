// Lamella managed corlib (from scratch). -- System.NumberFormatter (internal)
namespace System
{
    internal sealed class NumberFormatter
    {
        internal static string Format(long value, int nibbles, string format)
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

            if (specifier == 'G' || specifier == 'g') return Decimal(value, 0);
            if (specifier == 'D' || specifier == 'd') return Decimal(value, precision < 0 ? 0 : precision);
            if (specifier == 'X') return Hex(value, nibbles, precision, true);
            if (specifier == 'x') return Hex(value, nibbles, precision, false);
            if (specifier == 'N' || specifier == 'n') return Fixed(value, precision < 0 ? 2 : precision, true);
            if (specifier == 'F' || specifier == 'f') return Fixed(value, precision < 0 ? 2 : precision, false);
            throw new FormatException("Format specifier was invalid.");
        }

        private static bool IsLetter(char c)
        {
            return (c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z');
        }

        private static string Decimal(long value, int minDigits)
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

        private static string Fixed(long value, int decimals, bool grouped)
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
            if (grouped)
            {
                for (int k = 0; k < digitCount; k++)
                {
                    if (k != 0 && (digitCount - k) % 3 == 0) result.Append(',');
                    result.Append(buffer[pos + k]);
                }
            }
            else
            {
                for (int k = 0; k < digitCount; k++) result.Append(buffer[pos + k]);
            }
            if (decimals > 0)
            {
                result.Append('.');
                for (int k = 0; k < decimals; k++) result.Append('0');
            }
            return result.ToString();
        }
    }
}
