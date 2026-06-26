// Lamella managed corlib (from scratch). -- System.NumberFormatter (internal)
namespace System
{
    internal sealed class NumberFormatter
    {
        internal static string Format(long value, int nibbles, string format)
        {
            if (IsCustom(format))
            {
                return Custom(format, value < 0, true, value < 0 ? value : -value, 0.0);
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

            if (specifier == 'G' || specifier == 'g') return Decimal(value, 0);
            if (specifier == 'D' || specifier == 'd') return Decimal(value, precision < 0 ? 0 : precision);
            if (specifier == 'X') return Hex(value, nibbles, precision, true);
            if (specifier == 'x') return Hex(value, nibbles, precision, false);
            if (specifier == 'N' || specifier == 'n') return Fixed(value, precision < 0 ? 2 : precision, true);
            if (specifier == 'F' || specifier == 'f') return Fixed(value, precision < 0 ? 2 : precision, false);
            if (specifier == 'E' || specifier == 'e') return System.Double.ToExponential((double)value, precision < 0 ? 6 : precision, specifier == 'E');
            if (specifier == 'C' || specifier == 'c') return System.Double.Currency((double)value, precision < 0 ? 2 : precision);
            if (specifier == 'P' || specifier == 'p') return System.Double.Percent((double)value, precision < 0 ? 2 : precision);
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

        internal static bool IsCustom(string format)
        {
            if ((object)format == null || format.Length == 0) return false;
            if (!IsLetter(format[0])) return true;
            for (int i = 1; i < format.Length; i++)
            {
                if (format[i] < '0' || format[i] > '9') return true;
            }
            return false;
        }

        internal static string Custom(string format, bool negative, bool isInteger, long negMag, double magnitude)
        {
            bool isZero = isInteger ? (negMag == 0) : (magnitude == 0.0);
            string[] sections = SplitSections(format);
            string section;
            bool emitSign;
            if (sections.Length >= 3 && isZero) { section = sections[2]; emitSign = false; }
            else if (sections.Length >= 2 && negative) { section = sections[1]; emitSign = false; }
            else { section = sections[0]; emitSign = (sections.Length == 1) && negative; }
            if (section.Length == 0) { section = sections[0]; emitSign = negative; }

            int firstPlace = -1;
            int lastPlace = -1;
            for (int i = 0; i < section.Length; i++)
            {
                char c = section[i];
                if (c == '0' || c == '#') { if (firstPlace < 0) firstPlace = i; lastPlace = i; }
            }
            bool percent = section.IndexOf('%') >= 0;
            if (firstPlace < 0) return EmitLiterals(section);

            string prefix = section.Substring(0, firstPlace);
            string suffix = section.Substring(lastPlace + 1);
            string middle = section.Substring(firstPlace, lastPlace - firstPlace + 1);
            int dot = middle.IndexOf('.');
            string intRegion = (dot < 0) ? middle : middle.Substring(0, dot);
            string fracRegion = (dot < 0) ? "" : middle.Substring(dot + 1);
            int minInt = CountChar(intRegion, '0');
            bool grouping = intRegion.IndexOf(',') >= 0;
            int maxFrac = CountPlaceholders(fracRegion);
            int minFrac = MinFracDigits(fracRegion);

            string intDigits;
            string fracDigits;
            if (isInteger)
            {
                long m = negMag;
                if (percent) m = m * 100;
                intDigits = MagnitudeDecimal(m);
                fracDigits = Zeros(maxFrac);
            }
            else
            {
                double m = percent ? magnitude * 100.0 : magnitude;
                string fixedText = System.Double.ToFixed(m, maxFrac);
                int fdot = fixedText.IndexOf('.');
                if (fdot < 0) { intDigits = fixedText; fracDigits = ""; }
                else { intDigits = fixedText.Substring(0, fdot); fracDigits = fixedText.Substring(fdot + 1); }
            }

            if (minInt == 0 && IsAllZero(intDigits)) intDigits = "";
            else { while (intDigits.Length < minInt) intDigits = "0" + intDigits; }
            if (grouping && intDigits.Length > 0) intDigits = Group(intDigits);
            int keep = fracDigits.Length;
            while (keep > minFrac && fracDigits[keep - 1] == '0') keep = keep - 1;
            if (keep < fracDigits.Length) fracDigits = fracDigits.Substring(0, keep);

            System.Text.StringBuilder result = new System.Text.StringBuilder();
            if (emitSign) result.Append('-');
            result.Append(EmitLiterals(prefix));
            result.Append(intDigits);
            if (fracDigits.Length > 0) { result.Append('.'); result.Append(fracDigits); }
            result.Append(EmitLiterals(suffix));
            return result.ToString();
        }

        private static string MagnitudeDecimal(long negMag)
        {
            if (negMag == 0) return "0";
            char[] buffer = new char[24];
            int pos = buffer.Length;
            long n = negMag;
            while (n != 0)
            {
                int digit = (int)(-(n % 10));
                pos = pos - 1;
                buffer[pos] = (char)('0' + digit);
                n = n / 10;
            }
            System.Text.StringBuilder sb = new System.Text.StringBuilder();
            for (int i = pos; i < buffer.Length; i++) sb.Append(buffer[i]);
            return sb.ToString();
        }

        private static string[] SplitSections(string format)
        {
            int count = 1;
            for (int i = 0; i < format.Length; i++) if (format[i] == ';') count = count + 1;
            string[] result = new string[count];
            int start = 0;
            int index = 0;
            for (int i = 0; i < format.Length; i++)
            {
                if (format[i] == ';')
                {
                    result[index] = format.Substring(start, i - start);
                    index = index + 1;
                    start = i + 1;
                }
            }
            result[index] = format.Substring(start);
            return result;
        }

        private static int CountChar(string s, char target)
        {
            int count = 0;
            for (int i = 0; i < s.Length; i++) if (s[i] == target) count = count + 1;
            return count;
        }

        private static int CountPlaceholders(string s)
        {
            int count = 0;
            for (int i = 0; i < s.Length; i++) if (s[i] == '0' || s[i] == '#') count = count + 1;
            return count;
        }

        private static int MinFracDigits(string fracRegion)
        {
            int lastZero = -1;
            for (int i = 0; i < fracRegion.Length; i++) if (fracRegion[i] == '0') lastZero = i;
            return lastZero + 1;
        }

        private static string Zeros(int n)
        {
            System.Text.StringBuilder sb = new System.Text.StringBuilder();
            for (int i = 0; i < n; i++) sb.Append('0');
            return sb.ToString();
        }

        private static bool IsAllZero(string s)
        {
            for (int i = 0; i < s.Length; i++) if (s[i] != '0') return false;
            return true;
        }

        private static string Group(string digits)
        {
            System.Text.StringBuilder sb = new System.Text.StringBuilder();
            int len = digits.Length;
            for (int i = 0; i < len; i++)
            {
                if (i != 0 && (len - i) % 3 == 0) sb.Append(',');
                sb.Append(digits[i]);
            }
            return sb.ToString();
        }

        private static string EmitLiterals(string s)
        {
            System.Text.StringBuilder sb = new System.Text.StringBuilder();
            int i = 0;
            while (i < s.Length)
            {
                char c = s[i];
                if (c == '\\' && i + 1 < s.Length) { sb.Append(s[i + 1]); i = i + 2; }
                else if (c == '\'' || c == '"')
                {
                    char quote = c;
                    i = i + 1;
                    while (i < s.Length && s[i] != quote) { sb.Append(s[i]); i = i + 1; }
                    if (i < s.Length) i = i + 1;
                }
                else { sb.Append(c); i = i + 1; }
            }
            return sb.ToString();
        }
    }
}
