// Lamella managed corlib (from scratch). -- System.Guid
namespace System
{
#if LAMELLA_SURFACE_NETFX_2_0
    public struct Guid : IComparable, IFormattable
#else
    public struct Guid : IComparable
#endif
    {
        private int _a;
        private short _b;
        private short _c;
        private byte _d;
        private byte _e;
        private byte _f;
        private byte _g;
        private byte _h;
        private byte _i;
        private byte _j;
        private byte _k;

        public static readonly Guid Empty = new Guid();

        public Guid(int a, short b, short c, byte d, byte e, byte f, byte g, byte h, byte i, byte j, byte k)
        {
            _a = a;
            _b = b;
            _c = c;
            _d = d;
            _e = e;
            _f = f;
            _g = g;
            _h = h;
            _i = i;
            _j = j;
            _k = k;
        }

        public Guid(byte[] b)
        {
            if ((object)b == null) throw new ArgumentNullException("b");
            if (b.Length != 16) throw new ArgumentException("Byte array for GUID must be exactly 16 bytes long.");
            _a = b[0] | (b[1] << 8) | (b[2] << 16) | (b[3] << 24);
            _b = (short)(b[4] | (b[5] << 8));
            _c = (short)(b[6] | (b[7] << 8));
            _d = b[8];
            _e = b[9];
            _f = b[10];
            _g = b[11];
            _h = b[12];
            _i = b[13];
            _j = b[14];
            _k = b[15];
        }

        // Construct from D, N, B, P or X text. An overflowing 32-bit X field throws OverflowException.
        public Guid(string g)
        {
            if ((object)g == null) throw new ArgumentNullException("g");
            Guid parsed;
            if (!TryParseCore(g, true, out parsed))
                throw new FormatException("Guid should contain 32 digits with 4 dashes (xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx).");
            _a = parsed._a;
            _b = parsed._b;
            _c = parsed._c;
            _d = parsed._d;
            _e = parsed._e;
            _f = parsed._f;
            _g = parsed._g;
            _h = parsed._h;
            _i = parsed._i;
            _j = parsed._j;
            _k = parsed._k;
        }

        public byte[] ToByteArray()
        {
            byte[] result = new byte[16];
            result[0] = (byte)_a;
            result[1] = (byte)(_a >> 8);
            result[2] = (byte)(_a >> 16);
            result[3] = (byte)(_a >> 24);
            result[4] = (byte)_b;
            result[5] = (byte)(_b >> 8);
            result[6] = (byte)_c;
            result[7] = (byte)(_c >> 8);
            result[8] = _d;
            result[9] = _e;
            result[10] = _f;
            result[11] = _g;
            result[12] = _h;
            result[13] = _i;
            result[14] = _j;
            result[15] = _k;
            return result;
        }

        private static char HexDigit(int nibble)
        {
            if (nibble < 10) return (char)('0' + nibble);
            return (char)('a' + (nibble - 10));
        }

        private static void AppendHex(System.Text.StringBuilder builder, int value, int digits)
        {
            for (int i = digits - 1; i >= 0; i--)
            {
                int nibble = (value >> (4 * i)) & 0xF;
                builder.Append(HexDigit(nibble));
            }
        }

        private static void AppendByte(System.Text.StringBuilder builder, byte value)
        {
            int v = value;
            builder.Append(HexDigit((v >> 4) & 0xF));
            builder.Append(HexDigit(v & 0xF));
        }

        private void AppendBody(System.Text.StringBuilder builder)
        {
            AppendHex(builder, _a, 8);
            builder.Append('-');
            AppendHex(builder, _b & 0xFFFF, 4);
            builder.Append('-');
            AppendHex(builder, _c & 0xFFFF, 4);
            builder.Append('-');
            AppendByte(builder, _d);
            AppendByte(builder, _e);
            builder.Append('-');
            AppendByte(builder, _f);
            AppendByte(builder, _g);
            AppendByte(builder, _h);
            AppendByte(builder, _i);
            AppendByte(builder, _j);
            AppendByte(builder, _k);
        }

        public override string ToString()
        {
            System.Text.StringBuilder result = new System.Text.StringBuilder();
            AppendBody(result);
            return result.ToString();
        }

#if LAMELLA_SURFACE_NETFX_2_0
        /// <summary>Formats this value with <paramref name="format"/>. The provider is not used: nothing in
        /// a Guid's text depends on a culture.</summary>
        /// <param name="format">"D", "N", "B", "P" or "X", or null or empty for "D".</param>
        /// <param name="provider">Not used.</param>
        /// <returns>The text.</returns>
        public string ToString(string format, IFormatProvider provider)
        {
            return ToString(format);
        }
#endif

        private const string FormatRefusal = "Format string can be only \"D\", \"d\", \"N\", \"n\", \"P\", \"p\", \"B\", \"b\", \"X\" or \"x\".";

        public string ToString(string format)
        {
            char specifier = 'D';
            if ((object)format != null && format.Length != 0)
            {
                if (format.Length != 1) throw new FormatException(FormatRefusal);
                specifier = format[0];
            }
            System.Text.StringBuilder result = new System.Text.StringBuilder();
            if (specifier == 'N' || specifier == 'n')
            {
                AppendHex(result, _a, 8);
                AppendHex(result, _b & 0xFFFF, 4);
                AppendHex(result, _c & 0xFFFF, 4);
                AppendByte(result, _d);
                AppendByte(result, _e);
                AppendByte(result, _f);
                AppendByte(result, _g);
                AppendByte(result, _h);
                AppendByte(result, _i);
                AppendByte(result, _j);
                AppendByte(result, _k);
                return result.ToString();
            }
            if (specifier == 'B' || specifier == 'b')
            {
                result.Append('{');
                AppendBody(result);
                result.Append('}');
                return result.ToString();
            }
            if (specifier == 'P' || specifier == 'p')
            {
                result.Append('(');
                AppendBody(result);
                result.Append(')');
                return result.ToString();
            }
            if (specifier == 'D' || specifier == 'd')
            {
                AppendBody(result);
                return result.ToString();
            }
            if (specifier == 'X' || specifier == 'x')
            {
                result.Append("{0x");
                AppendHex(result, _a, 8);
                result.Append(",0x");
                AppendHex(result, _b & 0xFFFF, 4);
                result.Append(",0x");
                AppendHex(result, _c & 0xFFFF, 4);
                result.Append(",{0x");
                AppendByte(result, _d);
                result.Append(",0x");
                AppendByte(result, _e);
                result.Append(",0x");
                AppendByte(result, _f);
                result.Append(",0x");
                AppendByte(result, _g);
                result.Append(",0x");
                AppendByte(result, _h);
                result.Append(",0x");
                AppendByte(result, _i);
                result.Append(",0x");
                AppendByte(result, _j);
                result.Append(",0x");
                AppendByte(result, _k);
                result.Append("}}");
                return result.ToString();
            }
            throw new FormatException(FormatRefusal);
        }

        private static int HexValue(char c)
        {
            if (c >= '0' && c <= '9') return c - '0';
            if (c >= 'a' && c <= 'f') return c - 'a' + 10;
            if (c >= 'A' && c <= 'F') return c - 'A' + 10;
            return -1;
        }

        private static bool ReadHex(string s, ref int index, int count, out int value)
        {
            value = 0;
            if (index + count > s.Length) return false;
            int v = 0;
            for (int i = 0; i < count; i++)
            {
                int h = HexValue(s[index + i]);
                if (h < 0) return false;
                v = (v << 4) | h;
            }
            value = v;
            index = index + count;
            return true;
        }

        private static bool ReadChar(string s, ref int index, char expected)
        {
            if (index >= s.Length || s[index] != expected) return false;
            index = index + 1;
            return true;
        }

        private static void SkipXWhiteSpace(string s, ref int index, int end)
        {
            while (index < end && Char.IsWhiteSpace(s[index])) index++;
        }

        private static bool ReadXChar(string s, ref int index, int end, char expected)
        {
            SkipXWhiteSpace(s, ref index, end);
            if (index == end || s[index] != expected) return false;
            index++;
            return true;
        }

        private static bool ReadXPrefix(string s, ref int index, int end)
        {
            if (!ReadXChar(s, ref index, end, '0')) return false;
            SkipXWhiteSpace(s, ref index, end);
            if (index == end || (s[index] != 'x' && s[index] != 'X')) return false;
            index++;
            return true;
        }

        // X fields require 0x, then accept a leading plus and a second hex prefix. Whitespace is
        // insignificant even within a number. Find the delimiter before decoding to preserve the
        // distinction between a missing delimiter and an overflowing number in the constructor.
        private static bool ReadXNumber(string s, ref int index, int end, char delimiter,
            bool throwOnOverflow, out int value)
        {
            value = 0;
            if (!ReadXPrefix(s, ref index, end)) return false;
            int limit = index;
            while (limit < end && s[limit] != delimiter) limit++;
            if (limit == end) return false;
            SkipXWhiteSpace(s, ref index, limit);
            if (index == limit) return false;
            if (s[index] == '+') index++;
            int prefix = index;
            if (ReadXPrefix(s, ref prefix, limit)) index = prefix;
            uint number = 0;
            while (index < limit)
            {
                char ch = s[index];
                index++;
                if (Char.IsWhiteSpace(ch)) continue;
                int digit = HexValue(ch);
                if (digit < 0) return false;
                if (number > 0x0fffffff)
                {
                    if (throwOnOverflow) throw new OverflowException("Value was too large for a UInt32.");
                    return false;
                }
                number = (number << 4) | (uint)digit;
            }
            value = unchecked((int)number);
            index = limit + 1;
            return true;
        }

        private static bool ReadXByte(string s, ref int index, int end, char delimiter,
            bool throwOnOverflow, out int value)
        {
            if (!ReadXNumber(s, ref index, end, delimiter, throwOnOverflow, out value)) return false;
            return (uint)value <= 255;
        }

        private static bool TryParseX(string s, int index, int end, bool throwOnOverflow, out Guid result)
        {
            result = new Guid();
            int a, b, c, d, e, f, g, h, i, j, k;
            if (!ReadXChar(s, ref index, end, '{')) return false;
            if (!ReadXNumber(s, ref index, end, ',', throwOnOverflow, out a)) return false;
            if (!ReadXNumber(s, ref index, end, ',', throwOnOverflow, out b)) return false;
            if (!ReadXNumber(s, ref index, end, ',', throwOnOverflow, out c)) return false;
            if (!ReadXChar(s, ref index, end, '{')) return false;
            if (!ReadXByte(s, ref index, end, ',', throwOnOverflow, out d)) return false;
            if (!ReadXByte(s, ref index, end, ',', throwOnOverflow, out e)) return false;
            if (!ReadXByte(s, ref index, end, ',', throwOnOverflow, out f)) return false;
            if (!ReadXByte(s, ref index, end, ',', throwOnOverflow, out g)) return false;
            if (!ReadXByte(s, ref index, end, ',', throwOnOverflow, out h)) return false;
            if (!ReadXByte(s, ref index, end, ',', throwOnOverflow, out i)) return false;
            if (!ReadXByte(s, ref index, end, ',', throwOnOverflow, out j)) return false;
            if (!ReadXByte(s, ref index, end, '}', throwOnOverflow, out k)) return false;
            if (!ReadXChar(s, ref index, end, '}')) return false;
            SkipXWhiteSpace(s, ref index, end);
            if (index != end) return false;
            // The second and third X fields retain the low 16 bits of the parsed unsigned value.
            result = new Guid(a, unchecked((short)b), unchecked((short)c),
                (byte)d, (byte)e, (byte)f, (byte)g, (byte)h, (byte)i, (byte)j, (byte)k);
            return true;
        }

        // Parse, TryParse and the string constructor share the five formats and case-insensitive hex.
        // Only the constructor reports an overflowing 32-bit X field as OverflowException.
        private static bool TryParseCore(string s, bool throwOnOverflow, out Guid result)
        {
            result = new Guid();
            if ((object)s == null) return false;
            int start = 0;
            int end = s.Length;
            while (start < end && Char.IsWhiteSpace(s[start])) start = start + 1;
            while (end > start && Char.IsWhiteSpace(s[end - 1])) end = end - 1;
            int length = end - start;
            if (length == 0) return false;

            char open = s[start];
            if (open == '{')
            {
                int prefix = start + 1;
                if (ReadXPrefix(s, ref prefix, end))
                    return TryParseX(s, start, end, throwOnOverflow, out result);
            }
            char close = (char)0;
            bool braced = false;
            if (open == '{') { close = '}'; braced = true; start = start + 1; end = end - 1; }
            else if (open == '(') { close = ')'; braced = true; start = start + 1; end = end - 1; }
            if (braced)
            {
                if (end < start) return false;
                if (s[end] != close) return false;
            }

            int bodyLen = end - start;
            bool hyphenated = bodyLen >= 9 && s[start + 8] == '-';

            int idx = start;
            int a, b, c, d, e, f, gg, h, i, j, k;
            if (hyphenated)
            {
                if (!ReadHex(s, ref idx, 8, out a)) return false;
                if (!ReadChar(s, ref idx, '-')) return false;
                if (!ReadHex(s, ref idx, 4, out b)) return false;
                if (!ReadChar(s, ref idx, '-')) return false;
                if (!ReadHex(s, ref idx, 4, out c)) return false;
                if (!ReadChar(s, ref idx, '-')) return false;
                if (!ReadHex(s, ref idx, 2, out d)) return false;
                if (!ReadHex(s, ref idx, 2, out e)) return false;
                if (!ReadChar(s, ref idx, '-')) return false;
                if (!ReadHex(s, ref idx, 2, out f)) return false;
                if (!ReadHex(s, ref idx, 2, out gg)) return false;
                if (!ReadHex(s, ref idx, 2, out h)) return false;
                if (!ReadHex(s, ref idx, 2, out i)) return false;
                if (!ReadHex(s, ref idx, 2, out j)) return false;
                if (!ReadHex(s, ref idx, 2, out k)) return false;
            }
            else
            {
                if (!ReadHex(s, ref idx, 8, out a)) return false;
                if (!ReadHex(s, ref idx, 4, out b)) return false;
                if (!ReadHex(s, ref idx, 4, out c)) return false;
                if (!ReadHex(s, ref idx, 2, out d)) return false;
                if (!ReadHex(s, ref idx, 2, out e)) return false;
                if (!ReadHex(s, ref idx, 2, out f)) return false;
                if (!ReadHex(s, ref idx, 2, out gg)) return false;
                if (!ReadHex(s, ref idx, 2, out h)) return false;
                if (!ReadHex(s, ref idx, 2, out i)) return false;
                if (!ReadHex(s, ref idx, 2, out j)) return false;
                if (!ReadHex(s, ref idx, 2, out k)) return false;
            }
            if (idx != end) return false;

            result = new Guid(a, (short)b, (short)c, (byte)d, (byte)e, (byte)f, (byte)gg, (byte)h, (byte)i, (byte)j, (byte)k);
            return true;
        }

#if LAMELLA_SURFACE_NETFX_4_0
        public static Guid Parse(string input)
        {
            if ((object)input == null) throw new ArgumentNullException("input");
            Guid result;
            if (!TryParseCore(input, false, out result))
                throw new FormatException("Guid should contain 32 digits with 4 dashes (xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx).");
            return result;
        }

        public static bool TryParse(string input, out Guid result)
        {
            return TryParseCore(input, false, out result);
        }
#endif

        public bool Equals(Guid g)
        {
            return _a == g._a && _b == g._b && _c == g._c
                && _d == g._d && _e == g._e && _f == g._f && _g == g._g
                && _h == g._h && _i == g._i && _j == g._j && _k == g._k;
        }

        public override bool Equals(object obj)
        {
            if (!(obj is Guid)) return false;
            return Equals((Guid)obj);
        }

        public override int GetHashCode()
        {
            int word1 = _a;
            int word2 = (_b & 0xFFFF) | (_c << 16);
            int word3 = _d | (_e << 8) | (_f << 16) | (_g << 24);
            int word4 = _h | (_i << 8) | (_j << 16) | (_k << 24);
            return word1 ^ word2 ^ word3 ^ word4;
        }

        private static int CompareUInt(int left, int right)
        {
            int l = left ^ unchecked((int)0x80000000);
            int r = right ^ unchecked((int)0x80000000);
            if (l < r) return -1;
            if (l > r) return 1;
            return 0;
        }

        private static int CompareInt(int left, int right)
        {
            if (left < right) return -1;
            if (left > right) return 1;
            return 0;
        }

        public int CompareTo(Guid value)
        {
            int r = CompareUInt(_a, value._a);
            if (r != 0) return r;
            r = CompareInt(_b & 0xFFFF, value._b & 0xFFFF);
            if (r != 0) return r;
            r = CompareInt(_c & 0xFFFF, value._c & 0xFFFF);
            if (r != 0) return r;
            r = CompareInt(_d, value._d);
            if (r != 0) return r;
            r = CompareInt(_e, value._e);
            if (r != 0) return r;
            r = CompareInt(_f, value._f);
            if (r != 0) return r;
            r = CompareInt(_g, value._g);
            if (r != 0) return r;
            r = CompareInt(_h, value._h);
            if (r != 0) return r;
            r = CompareInt(_i, value._i);
            if (r != 0) return r;
            r = CompareInt(_j, value._j);
            if (r != 0) return r;
            r = CompareInt(_k, value._k);
            return r;
        }

        public int CompareTo(object obj)
        {
            if (obj == null) return 1;
            if (!(obj is Guid)) throw new ArgumentException("Object must be of type GUID.");
            return CompareTo((Guid)obj);
        }

        public static bool operator ==(Guid a, Guid b) { return a.Equals(b); }
        public static bool operator !=(Guid a, Guid b) { return !a.Equals(b); }
    }
}
