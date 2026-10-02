// Lamella managed corlib (from scratch). -- System.Text.UTF8Encoding
namespace System.Text
{
    public class UTF8Encoding : Encoding
    {
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

        private static int ScalarAt(string s, int i)
        {
            int c = s[i];
            if (IsHighSurrogate(c) && i + 1 < s.Length && IsLowSurrogate(s[i + 1]))
            {
                return 0x10000 + ((c - 0xD800) << 10) + (s[i + 1] - 0xDC00);
            }
            if (IsHighSurrogate(c) || IsLowSurrogate(c)) return 0xFFFD;
            return c;
        }

        public override int GetByteCount(string s)
        {
            int count = 0;
            int i = 0;
            while (i < s.Length)
            {
                int scalar = ScalarAt(s, i);
                count = count + BytesForScalar(scalar);
                i = i + (scalar >= 0x10000 ? 2 : 1);
            }
            return count;
        }

        public override byte[] GetBytes(string s)
        {
            byte[] bytes = new byte[GetByteCount(s)];
            int pos = 0;
            int i = 0;
            while (i < s.Length)
            {
                int cp = ScalarAt(s, i);
                i = i + (cp >= 0x10000 ? 2 : 1);
                if (cp < 0x80)
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
            return bytes;
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
            if (bytes == null) throw new ArgumentNullException("bytes", "Array cannot be null.");
            if (index < 0) throw new ArgumentOutOfRangeException("index", "Non-negative number required.");
            if (count < 0) throw new ArgumentOutOfRangeException("count", "Non-negative number required.");
            RequireRangeWithin(bytes, index, count);

            StringBuilder result = new StringBuilder(count);
            int end = index + count;
            int i = index;
            while (i < end)
            {
                int lead = bytes[i];
                if (lead < 0x80)
                {
                    result.Append((char)lead);
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
                    result.Append(ReplacementCharacter);
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
                    result.Append(ReplacementCharacter);
                }
                else if (scalar >= 0x10000)
                {
                    int offset = scalar - 0x10000;
                    result.Append((char)(0xD800 + (offset >> 10)));
                    result.Append((char)(0xDC00 + (offset & 0x3FF)));
                }
                else
                {
                    result.Append((char)scalar);
                }
            }
            return result.ToString();
        }

    }
}
