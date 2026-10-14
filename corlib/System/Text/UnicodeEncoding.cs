// Lamella managed corlib (from scratch). -- System.Text.UnicodeEncoding
namespace System.Text
{
    public class UnicodeEncoding : Encoding
    {
        internal static UnicodeEncoding _default = new UnicodeEncoding();

        public UnicodeEncoding() { }

        private static void RequireNonNegativeValue(int value, string name)
        {
            if (value < 0)
            {
                string text = Int32.Decimal(value);
                throw new ArgumentOutOfRangeException(name, text, name + " ('" + text + "') must be a non-negative value.");
            }
        }

        public override int GetByteCount(string s) { return s.Length * 2; }

        public override int GetByteCount(char[] chars, int index, int count)
        {
            if (chars == null) throw new ArgumentNullException("chars");
            RequireNonNegativeValue(index, "index");
            RequireNonNegativeValue(count, "count");
            RequireRangeWithin(chars, index, count, "buffer");
            return count * 2;
        }

        public override byte[] GetBytes(string s)
        {
            byte[] bytes = new byte[s.Length * 2];
            Encode(s, null, 0, s.Length, bytes, 0);
            return bytes;
        }

        public override int GetBytes(char[] chars, int charIndex, int charCount, byte[] bytes, int byteIndex)
        {
            if (chars == null) throw new ArgumentNullException("chars");
            if (bytes == null) throw new ArgumentNullException("bytes");
            RequireNonNegativeValue(charIndex, "charIndex");
            RequireNonNegativeValue(charCount, "charCount");
            RequireRangeWithin(chars, charIndex, charCount, "buffer");
            RequireDestinationIndex(byteIndex, bytes.Length, "byteIndex");
            RequireByteRoom(charCount * 2, bytes, byteIndex, "1200");
            Encode(null, chars, charIndex, charCount, bytes, byteIndex);
            return charCount * 2;
        }

        private static void Encode(string s, char[] a, int index, int count, byte[] bytes, int byteIndex)
        {
            int end = index + count;
            int pos = byteIndex;
            int i = index;
            while (i < end)
            {
                int c = a != null ? a[i] : s[i];
                i = i + 1;
                if (c >= 0xD800 && c <= 0xDBFF && i < end)
                {
                    int next = a != null ? a[i] : s[i];
                    if (next >= 0xDC00 && next <= 0xDFFF)
                    {
                        bytes[pos] = (byte)(c & 0xFF);
                        bytes[pos + 1] = (byte)((c >> 8) & 0xFF);
                        bytes[pos + 2] = (byte)(next & 0xFF);
                        bytes[pos + 3] = (byte)((next >> 8) & 0xFF);
                        pos = pos + 4;
                        i = i + 1;
                        continue;
                    }
                }
                if (c >= 0xD800 && c <= 0xDFFF) c = ReplacementCharacter;
                bytes[pos] = (byte)(c & 0xFF);
                bytes[pos + 1] = (byte)((c >> 8) & 0xFF);
                pos = pos + 2;
            }
        }

        private static int Decode(byte[] bytes, int index, int count, char[] chars, int charIndex)
        {
            int at = charIndex;
            int end = index + count;
            int i = index;
            while (i + 1 < end)
            {
                int unit = bytes[i] | (bytes[i + 1] << 8);
                i = i + 2;
                if (unit >= 0xD800 && unit <= 0xDBFF)
                {
                    if (i + 1 < end)
                    {
                        int next = bytes[i] | (bytes[i + 1] << 8);
                        if (next >= 0xDC00 && next <= 0xDFFF)
                        {
                            if (chars != null)
                            {
                                chars[at] = (char)unit;
                                chars[at + 1] = (char)next;
                            }
                            at = at + 2;
                            i = i + 2;
                            continue;
                        }
                    }
                    unit = ReplacementCharacter;
                }
                else if (unit >= 0xDC00 && unit <= 0xDFFF)
                {
                    unit = ReplacementCharacter;
                }
                if (chars != null) chars[at] = (char)unit;
                at = at + 1;
            }
            if (i < end)
            {
                if (chars != null) chars[at] = ReplacementCharacter;
                at = at + 1;
            }
            return at - charIndex;
        }

        public override int GetCharCount(byte[] bytes, int index, int count)
        {
            if (bytes == null) throw new ArgumentNullException("bytes");
            RequireNonNegativeValue(index, "index");
            RequireNonNegativeValue(count, "count");
            RequireRangeWithin(bytes, index, count);
            return Decode(bytes, index, count, null, 0);
        }

        public override int GetChars(byte[] bytes, int byteIndex, int byteCount, char[] chars, int charIndex)
        {
            if (bytes == null) throw new ArgumentNullException("bytes");
            if (chars == null) throw new ArgumentNullException("chars");
            RequireNonNegativeValue(byteIndex, "byteIndex");
            RequireNonNegativeValue(byteCount, "byteCount");
            RequireRangeWithin(bytes, byteIndex, byteCount);
            RequireDestinationIndex(charIndex, chars.Length, "charIndex");
            RequireCharRoom(Decode(bytes, byteIndex, byteCount, null, 0), chars, charIndex, "1200");
            return Decode(bytes, byteIndex, byteCount, chars, charIndex);
        }

        /// <summary>Decodes a range of UTF-16 little-endian bytes into a string. A lone surrogate, and a
        /// final byte with no partner, decode to U+FFFD.</summary>
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
            if (bytes == null) throw new ArgumentNullException("bytes");
            RequireNonNegativeValue(index, "index");
            RequireNonNegativeValue(count, "count");
            RequireRangeWithin(bytes, index, count);
            char[] chars = new char[Decode(bytes, index, count, null, 0)];
            Decode(bytes, index, count, chars, 0);
            return new string(chars);
        }
    }
}
