// Lamella managed corlib (from scratch). -- System.NumberFormatter (internal)
namespace System
{
    internal sealed class NumberFormatter
    {
        internal static string Format(long value, int nibbles, string format)
        {
            if (IsCustom(format))
            {
                return CustomInteger(format, value < 0, value < 0 ? value : -value);
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
#if LAMELLA_SURFACE_FLOAT
            if (specifier == 'E' || specifier == 'e' || specifier == 'C' || specifier == 'c' || specifier == 'P' || specifier == 'p')
            {
                return FormatScaled(value, specifier, precision);
            }
#endif
            throw new FormatException("Format specifier was invalid.");
        }

#if LAMELLA_SURFACE_FLOAT
        private static string FormatScaled(long value, char specifier, int precision)
        {
            if (specifier == 'E' || specifier == 'e') return System.Double.ToExponential((double)value, precision < 0 ? 6 : precision, specifier == 'E');
            if (specifier == 'C' || specifier == 'c') return System.Double.Currency((double)value, precision < 0 ? 2 : precision);
            return System.Double.Percent((double)value, precision < 0 ? 2 : precision);
        }
#endif

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

        private sealed class CustomShape
        {
            internal string Section;
            internal bool EmitSign;
            internal bool HasPlaces;
            internal bool Percent;
            internal string Prefix;
            internal string Suffix;
            internal string IntTemplate;
            internal string FracTemplate;
            internal int MinInt;
            internal bool Grouping;
            internal int MaxFrac;
            internal int MinFrac;
            internal int Scale;
        }

        private static CustomShape ParseCustom(string format, bool negative, bool isZero)
        {
            string[] sections = SplitSections(format);
            string section;
            bool emitSign;
            if (sections.Length >= 3 && isZero) { section = sections[2]; emitSign = false; }
            else if (sections.Length >= 2 && negative) { section = sections[1]; emitSign = false; }
            else { section = sections[0]; emitSign = (sections.Length == 1) && negative; }
            if (section.Length == 0) { section = sections[0]; emitSign = negative; }

            CustomShape shape = new CustomShape();
            shape.Section = section;
            shape.EmitSign = emitSign;
            int firstPlace = -1;
            int lastPlace = -1;
            for (int i = 0; i < section.Length; i++)
            {
                char c = section[i];
                if (c == '0' || c == '#') { if (firstPlace < 0) firstPlace = i; lastPlace = i; }
            }
            shape.Percent = section.IndexOf('%') >= 0;
            if (firstPlace < 0)
            {
                shape.HasPlaces = false;
                return shape;
            }
            shape.HasPlaces = true;
            shape.Prefix = section.Substring(0, firstPlace);
            shape.Suffix = section.Substring(lastPlace + 1);
            string middle = section.Substring(firstPlace, lastPlace - firstPlace + 1);
            int dot = middle.IndexOf('.');
            string intRegion = (dot < 0) ? middle : middle.Substring(0, dot);
            string fracRegion = (dot < 0) ? "" : middle.Substring(dot + 1);
            int end = intRegion.Length;
            while (end > 0 && intRegion[end - 1] == ',') { shape.Scale = shape.Scale + 1; end = end - 1; }
            intRegion = intRegion.Substring(0, end);
            if (dot < 0)
            {
                int lead = 0;
                while (lead < shape.Suffix.Length && shape.Suffix[lead] == ',')
                {
                    shape.Scale = shape.Scale + 1;
                    lead = lead + 1;
                }
                if (lead > 0) shape.Suffix = shape.Suffix.Substring(lead);
            }
            shape.IntTemplate = intRegion;
            shape.FracTemplate = fracRegion;
            shape.MinInt = CountChar(intRegion, '0');
            shape.Grouping = intRegion.IndexOf(',') >= 0;
            shape.MaxFrac = CountPlaceholders(fracRegion);
            shape.MinFrac = MinFracDigits(fracRegion);
            return shape;
        }

        internal static string CustomInteger(string format, bool negative, long negMag)
        {
            CustomShape shape = ParseCustom(format, negative, negMag == 0);
            if (!shape.HasPlaces) return EmitLiterals(shape.Section);
            long m = negMag;
            if (shape.Percent) m = m * 100;
            for (int s = 0; s < shape.Scale; s++) m = (m - 500) / 1000;
            return AssembleCustom(shape, MagnitudeDecimal(m), Zeros(shape.MaxFrac));
        }

#if LAMELLA_SURFACE_FLOAT
        internal static string CustomFloat(string format, bool negative, double magnitude)
        {
            CustomShape shape = ParseCustom(format, negative, magnitude == 0.0);
            if (!shape.HasPlaces) return EmitLiterals(shape.Section);
            double m = shape.Percent ? magnitude * 100.0 : magnitude;
            for (int s = 0; s < shape.Scale; s++) m = m / 1000.0;
            string fixedText = RoundPlainDecimal(Significant15(m), shape.MaxFrac);
            int fdot = fixedText.IndexOf('.');
            string intDigits;
            string fracDigits;
            if (fdot < 0) { intDigits = fixedText; fracDigits = ""; }
            else { intDigits = fixedText.Substring(0, fdot); fracDigits = fixedText.Substring(fdot + 1); }
            return AssembleCustom(shape, intDigits, fracDigits);
        }

        private static string Significant15(double magnitude)
        {
            string text = System.Double.ToExponential(magnitude, 14, true);
            int marker = text.IndexOf('E');
            if (marker < 0) return text;
            string mantissa = text.Substring(0, marker);
            int exponent = ParseSignedInt(text.Substring(marker + 1));
            int dot = mantissa.IndexOf('.');
            string digits = (dot < 0)
                ? mantissa
                : mantissa.Substring(0, dot) + mantissa.Substring(dot + 1);

            int point = exponent + 1;
            System.Text.StringBuilder sb = new System.Text.StringBuilder();
            if (point <= 0)
            {
                sb.Append("0.");
                for (int i = 0; i < -point; i++) sb.Append('0');
                sb.Append(digits);
            }
            else if (point >= digits.Length)
            {
                sb.Append(digits);
                for (int i = digits.Length; i < point; i++) sb.Append('0');
            }
            else
            {
                sb.Append(digits.Substring(0, point));
                sb.Append('.');
                sb.Append(digits.Substring(point));
            }
            return sb.ToString();
        }

        private static string RoundPlainDecimal(string plain, int frac)
        {
            int dot = plain.IndexOf('.');
            if (dot < 0 && !IsDigits(plain)) return plain;
            string whole = (dot < 0) ? plain : plain.Substring(0, dot);
            string fraction = (dot < 0) ? "" : plain.Substring(dot + 1);
            if (fraction.Length <= frac)
            {
                System.Text.StringBuilder pad = new System.Text.StringBuilder(fraction);
                while (pad.Length < frac) pad.Append('0');
                fraction = pad.ToString();
            }
            else
            {
                bool up = fraction[frac] >= '5';
                fraction = fraction.Substring(0, frac);
                if (up)
                {
                    string carried = AddOneToLastDigit(whole + fraction);
                    whole = carried.Substring(0, carried.Length - frac);
                    fraction = carried.Substring(carried.Length - frac);
                }
            }
            return (frac == 0) ? whole : (whole + "." + fraction);
        }

        private static string AddOneToLastDigit(string digits)
        {
            System.Text.StringBuilder sb = new System.Text.StringBuilder(digits);
            int i = sb.Length - 1;
            while (i >= 0)
            {
                if (sb[i] == '9') { sb[i] = '0'; i = i - 1; continue; }
                sb[i] = (char)(sb[i] + 1);
                return sb.ToString();
            }
            return "1" + sb.ToString();
        }

        private static int ParseSignedInt(string s)
        {
            bool negative = s.Length > 0 && s[0] == '-';
            int value = 0;
            for (int i = 0; i < s.Length; i++)
            {
                char c = s[i];
                if (c < '0' || c > '9') continue;
                value = value * 10 + (c - '0');
            }
            return negative ? -value : value;
        }

        private static bool IsDigits(string s)
        {
            if (s.Length == 0) return false;
            for (int i = 0; i < s.Length; i++) if (s[i] < '0' || s[i] > '9') return false;
            return true;
        }
#endif

        private static string AssembleCustom(CustomShape shape, string intDigits, string fracDigits)
        {
            if (shape.MinInt == 0 && IsAllZero(intDigits)) intDigits = "";
            else { while (intDigits.Length < shape.MinInt) intDigits = "0" + intDigits; }
            if (shape.Grouping && intDigits.Length > 0) intDigits = Group(intDigits);
            int keep = fracDigits.Length;
            while (keep > shape.MinFrac && fracDigits[keep - 1] == '0') keep = keep - 1;
            if (keep < fracDigits.Length) fracDigits = fracDigits.Substring(0, keep);

            System.Text.StringBuilder result = new System.Text.StringBuilder();
            if (shape.EmitSign) result.Append('-');
            result.Append(EmitLiterals(shape.Prefix));
            result.Append(FillIntTemplate(shape.IntTemplate, intDigits));
            string fraction = FillFracTemplate(shape.FracTemplate, fracDigits);
            if (fraction.Length > 0) { result.Append('.'); result.Append(fraction); }
            result.Append(EmitLiterals(shape.Suffix));
            return result.ToString();
        }

        private static string FillIntTemplate(string template, string digits)
        {
            int places = CountPlaceholders(template);
            int remaining = CountDigits(digits);
            int skip = places - remaining;
            if (skip < 0) skip = 0;

            System.Text.StringBuilder sb = new System.Text.StringBuilder();
            System.Text.StringBuilder literal = new System.Text.StringBuilder();
            int cursor = 0;
            int index = 0;
            while (index < template.Length)
            {
                char c = template[index];
                if (c != '0' && c != '#') { literal.Append(c); index = index + 1; continue; }
                index = index + 1;
                if (skip > 0) { skip = skip - 1; continue; }
                sb.Append(EmitLiterals(literal.ToString()));
                literal = new System.Text.StringBuilder();
                places = places - 1;
                int take = remaining - places;
                if (take < 1) take = 1;
                remaining = remaining - take;
                while (take > 0 && cursor < digits.Length)
                {
                    char d = digits[cursor];
                    sb.Append(d);
                    cursor = cursor + 1;
                    if (d != ',') take = take - 1;
                }
                while (cursor < digits.Length && digits[cursor] == ',')
                {
                    sb.Append(digits[cursor]);
                    cursor = cursor + 1;
                }
            }
            sb.Append(EmitLiterals(literal.ToString()));
            while (cursor < digits.Length) { sb.Append(digits[cursor]); cursor = cursor + 1; }
            return sb.ToString();
        }

        private static string FillFracTemplate(string template, string digits)
        {
            System.Text.StringBuilder sb = new System.Text.StringBuilder();
            System.Text.StringBuilder literal = new System.Text.StringBuilder();
            int cursor = 0;
            int index = 0;
            while (index < template.Length)
            {
                char c = template[index];
                if (c != '0' && c != '#') { literal.Append(c); index = index + 1; continue; }
                index = index + 1;
                sb.Append(EmitLiterals(literal.ToString()));
                literal = new System.Text.StringBuilder();
                if (cursor >= digits.Length) break;
                sb.Append(digits[cursor]);
                cursor = cursor + 1;
            }
            return sb.ToString();
        }

        private static int CountDigits(string s)
        {
            int count = 0;
            for (int i = 0; i < s.Length; i++) if (s[i] != ',') count = count + 1;
            return count;
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

        private static bool SectionBreakAt(string format, int i, ref int skip)
        {
            char c = format[i];
            if (c == '\\') { skip = 2; return false; }
            if (c == '\'' || c == '"')
            {
                int j = i + 1;
                while (j < format.Length && format[j] != c) j = j + 1;
                skip = (j < format.Length) ? (j - i + 1) : (format.Length - i);
                return false;
            }
            skip = 1;
            return c == ';';
        }

        private static string[] SplitSections(string format)
        {
            int count = 1;
            int skip = 1;
            for (int i = 0; i < format.Length; i = i + skip)
            {
                if (SectionBreakAt(format, i, ref skip)) count = count + 1;
            }
            string[] result = new string[count];
            int start = 0;
            int index = 0;
            skip = 1;
            for (int i = 0; i < format.Length; i = i + skip)
            {
                if (SectionBreakAt(format, i, ref skip))
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
            int forced = 0;
            int places = 0;
            for (int i = 0; i < fracRegion.Length; i++)
            {
                char c = fracRegion[i];
                if (c != '0' && c != '#') continue;
                places = places + 1;
                if (c == '0') forced = places;
            }
            return forced;
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
                else if (c == ',') { i = i + 1; }
                else { sb.Append(c); i = i + 1; }
            }
            return sb.ToString();
        }
    }
}
