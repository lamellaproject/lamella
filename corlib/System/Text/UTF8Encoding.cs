// Lamella managed corlib (from scratch). -- System.Text.UTF8Encoding
namespace System.Text
{
    public class UTF8Encoding : Encoding
    {
        internal static UTF8Encoding _default = new UTF8Encoding();

        public UTF8Encoding() { }

        private static bool IsHighSurrogate(int c) { return c >= 0xD800 && c <= 0xDBFF; }
        private static bool IsLowSurrogate(int c) { return c >= 0xDC00 && c <= 0xDFFF; }

        private static int BytesForScalar(int cp)
        {
            if (cp < 0x80) return 1;
            if (cp < 0x800) return 2;
            if (cp < 0x10000) return 3;
            return 4;
        }

        private static int ScalarAt(string s, char[] a, int i, int end)
        {
            int c = a != null ? a[i] : s[i];
            if (IsHighSurrogate(c) && i + 1 < end)
            {
                int next = a != null ? a[i + 1] : s[i + 1];
                if (IsLowSurrogate(next)) return 0x10000 + ((c - 0xD800) << 10) + (next - 0xDC00);
            }
            if (IsHighSurrogate(c) || IsLowSurrogate(c)) return 0xFFFD;
            return c;
        }

        private static int Encode(string s, char[] a, int index, int count, byte[] bytes, int byteIndex)
        {
            int pos = byteIndex;
            int end = index + count;
            int i = index;
            while (i < end)
            {
                int cp = ScalarAt(s, a, i, end);
                i = i + (cp >= 0x10000 ? 2 : 1);
                if (bytes == null)
                {
                    pos = pos + BytesForScalar(cp);
                }
                else if (cp < 0x80)
                {
                    bytes[pos] = (byte)cp;
                    pos = pos + 1;
                }
                else if (cp < 0x800)
                {
                    bytes[pos] = (byte)(0xC0 | (cp >> 6));
                    bytes[pos + 1] = (byte)(0x80 | (cp & 0x3F));
                    pos = pos + 2;
                }
                else if (cp < 0x10000)
                {
                    bytes[pos] = (byte)(0xE0 | (cp >> 12));
                    bytes[pos + 1] = (byte)(0x80 | ((cp >> 6) & 0x3F));
                    bytes[pos + 2] = (byte)(0x80 | (cp & 0x3F));
                    pos = pos + 3;
                }
                else
                {
                    bytes[pos] = (byte)(0xF0 | (cp >> 18));
                    bytes[pos + 1] = (byte)(0x80 | ((cp >> 12) & 0x3F));
                    bytes[pos + 2] = (byte)(0x80 | ((cp >> 6) & 0x3F));
                    bytes[pos + 3] = (byte)(0x80 | (cp & 0x3F));
                    pos = pos + 4;
                }
            }
            return pos - byteIndex;
        }

        public override int GetByteCount(string s)
        {
            return Encode(s, null, 0, s.Length, null, 0);
        }

        public override int GetByteCount(char[] chars, int index, int count)
        {
            RequireArray(chars, "chars");
            RequireNonNegative(index, "index", count, "count");
            RequireRangeWithin(chars, index, count, "buffer");
            return Encode(null, chars, index, count, null, 0);
        }

        public override byte[] GetBytes(string s)
        {
            byte[] bytes = new byte[Encode(s, null, 0, s.Length, null, 0)];
            Encode(s, null, 0, s.Length, bytes, 0);
            return bytes;
        }

        public override int GetBytes(char[] chars, int charIndex, int charCount, byte[] bytes, int byteIndex)
        {
            RequireArray(chars, "chars");
            RequireArray(bytes, "bytes");
            RequireNonNegative(charIndex, "charIndex", charCount, "charCount");
            RequireRangeWithin(chars, charIndex, charCount, "string");
            RequireDestinationIndex(byteIndex, bytes.Length, "byteIndex");
            RequireByteRoom(Encode(null, chars, charIndex, charCount, null, 0), bytes, byteIndex, "65001");
            return Encode(null, chars, charIndex, charCount, bytes, byteIndex);
        }

        private static int Decode(byte[] bytes, int index, int count, char[] chars, int charIndex)
        {
            int at = charIndex;
            int end = index + count;
            int i = index;
            while (i < end)
            {
                int lead = bytes[i];
                if (lead < 0x80)
                {
                    if (chars != null) chars[at] = (char)lead;
                    at = at + 1;
                    i = i + 1;
                    continue;
                }

                int length;
                int scalar;
                int low = 0x80;
                int high = 0xBF;
                if (lead >= 0xC2 && lead <= 0xDF)
                {
                    length = 2;
                    scalar = lead & 0x1F;
                }
                else if (lead >= 0xE0 && lead <= 0xEF)
                {
                    length = 3;
                    scalar = lead & 0x0F;
                    if (lead == 0xE0) low = 0xA0;
                    else if (lead == 0xED) high = 0x9F;
                }
                else if (lead >= 0xF0 && lead <= 0xF4)
                {
                    length = 4;
                    scalar = lead & 0x07;
                    if (lead == 0xF0) low = 0x90;
                    else if (lead == 0xF4) high = 0x8F;
                }
                else
                {
                    if (chars != null) chars[at] = ReplacementCharacter;
                    at = at + 1;
                    i = i + 1;
                    continue;
                }

                int taken = 1;
                while (taken < length && i + taken < end)
                {
                    int next = bytes[i + taken];
                    if (next < low || next > high) break;
                    scalar = (scalar << 6) | (next & 0x3F);
                    taken = taken + 1;
                    low = 0x80;
                    high = 0xBF;
                }
                i = i + taken;
                if (taken < length)
                {
                    if (chars != null) chars[at] = ReplacementCharacter;
                    at = at + 1;
                }
                else if (scalar >= 0x10000)
                {
                    int offset = scalar - 0x10000;
                    if (chars != null)
                    {
                        chars[at] = (char)(0xD800 + (offset >> 10));
                        chars[at + 1] = (char)(0xDC00 + (offset & 0x3FF));
                    }
                    at = at + 2;
                }
                else
                {
                    if (chars != null) chars[at] = (char)scalar;
                    at = at + 1;
                }
            }
            return at - charIndex;
        }

        public override int GetCharCount(byte[] bytes, int index, int count)
        {
            RequireArray(bytes, "bytes");
            RequireNonNegative(index, "index", count, "count");
            RequireRangeWithin(bytes, index, count);
            return Decode(bytes, index, count, null, 0);
        }

        public override int GetChars(byte[] bytes, int byteIndex, int byteCount, char[] chars, int charIndex)
        {
            RequireArray(bytes, "bytes");
            RequireArray(chars, "chars");
            RequireNonNegative(byteIndex, "byteIndex", byteCount, "byteCount");
            RequireRangeWithin(bytes, byteIndex, byteCount);
            RequireDestinationIndex(charIndex, chars.Length, "charIndex");
            RequireCharRoom(Decode(bytes, byteIndex, byteCount, null, 0), chars, charIndex, "65001");
            return Decode(bytes, byteIndex, byteCount, chars, charIndex);
        }

        /// <summary>Decodes a range of UTF-8 bytes into a string. A byte sequence that is not well-formed
        /// UTF-8 decodes to U+FFFD, one for each maximal ill-formed subpart.</summary>
        /// <param name="bytes">The array holding the bytes to decode.</param>
        /// <param name="index">The position of the first byte to decode.</param>
        /// <param name="count">How many bytes to decode.</param>
        /// <returns>The decoded text.</returns>
        /// <exception cref="ArgumentNullException"><paramref name="bytes"/> is null.</exception>
        /// <exception cref="ArgumentOutOfRangeException"><paramref name="index"/> or
        /// <paramref name="count"/> is negative, or the range does not lie within
        /// <paramref name="bytes"/>.</exception>
        public override string GetString(byte[] bytes, int index, int count)
        {
            RequireArray(bytes, "bytes");
            RequireNonNegative(index, "index", count, "count");
            RequireRangeWithin(bytes, index, count);
            char[] chars = new char[Decode(bytes, index, count, null, 0)];
            Decode(bytes, index, count, chars, 0);
            return new string(chars);
        }
    }
}
