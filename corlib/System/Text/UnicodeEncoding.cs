// Lamella managed corlib (from scratch). -- System.Text.UnicodeEncoding
namespace System.Text
{
    public class UnicodeEncoding : Encoding
    {
        public UnicodeEncoding() { }

        public override int GetByteCount(string s) { return s.Length * 2; }

        public override byte[] GetBytes(string s)
        {
            byte[] bytes = new byte[s.Length * 2];
            for (int i = 0; i < s.Length; i++)
            {
                int c = s[i];
                bytes[2 * i] = (byte)(c & 0xFF);
                bytes[2 * i + 1] = (byte)((c >> 8) & 0xFF);
            }
            return bytes;
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
            if (index < 0)
            {
                string text = Int32.Decimal(index);
                throw new ArgumentOutOfRangeException("index", text, "index ('" + text + "') must be a non-negative value.");
            }
            if (count < 0)
            {
                string text = Int32.Decimal(count);
                throw new ArgumentOutOfRangeException("count", text, "count ('" + text + "') must be a non-negative value.");
            }
            RequireRangeWithin(bytes, index, count);

            StringBuilder result = new StringBuilder(count / 2 + 1);
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
                            result.Append((char)unit);
                            result.Append((char)next);
                            i = i + 2;
                            continue;
                        }
                    }
                    result.Append(ReplacementCharacter);
                }
                else if (unit >= 0xDC00 && unit <= 0xDFFF)
                {
                    result.Append(ReplacementCharacter);
                }
                else
                {
                    result.Append((char)unit);
                }
            }
            if (i < end) result.Append(ReplacementCharacter);
            return result.ToString();
        }
    }
}
