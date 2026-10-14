// Lamella managed corlib (from scratch). -- System.Text.ASCIIEncoding
namespace System.Text
{
    public class ASCIIEncoding : Encoding
    {
        internal static ASCIIEncoding _default = new ASCIIEncoding();

        public ASCIIEncoding() { }

        public override int GetByteCount(string s) { return s.Length; }

        public override int GetByteCount(char[] chars, int index, int count)
        {
            RequireArray(chars, "chars");
            RequireNonNegative(index, "index", count, "count");
            RequireRangeWithin(chars, index, count, "buffer");
            return count;
        }

        public override byte[] GetBytes(string s)
        {
            byte[] bytes = new byte[s.Length];
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
            RequireByteRoom(charCount, bytes, byteIndex, "20127");
            Encode(null, chars, charIndex, charCount, bytes, byteIndex);
            return charCount;
        }

        private static void Encode(string s, char[] a, int index, int count, byte[] bytes, int byteIndex)
        {
            for (int i = 0; i < count; i++)
            {
                int c = a != null ? a[index + i] : s[index + i];
                bytes[byteIndex + i] = (byte)(c <= 0x7F ? c : '?');
            }
        }

        public override int GetCharCount(byte[] bytes, int index, int count)
        {
            RequireArray(bytes, "bytes");
            RequireNonNegative(index, "index", count, "count");
            RequireRangeWithin(bytes, index, count);
            return count;
        }

        public override int GetChars(byte[] bytes, int byteIndex, int byteCount, char[] chars, int charIndex)
        {
            RequireArray(bytes, "bytes");
            RequireArray(chars, "chars");
            RequireNonNegative(byteIndex, "byteIndex", byteCount, "byteCount");
            RequireRangeWithin(bytes, byteIndex, byteCount);
            RequireDestinationIndex(charIndex, chars.Length, "charIndex");
            RequireCharRoom(byteCount, chars, charIndex, "20127");
            Decode(bytes, byteIndex, byteCount, chars, charIndex);
            return byteCount;
        }

        /// <summary>Decodes a range of ASCII bytes into a string. A byte above 0x7F decodes to
        /// '?'.</summary>
        /// <param name="bytes">The array holding the bytes to decode.</param>
        /// <param name="byteIndex">The position of the first byte to decode.</param>
        /// <param name="byteCount">How many bytes to decode.</param>
        /// <returns>The decoded text.</returns>
        /// <exception cref="ArgumentNullException"><paramref name="bytes"/> is null.</exception>
        /// <exception cref="ArgumentOutOfRangeException"><paramref name="byteIndex"/> or
        /// <paramref name="byteCount"/> is negative, or the range does not lie within
        /// <paramref name="bytes"/>.</exception>
        public override string GetString(byte[] bytes, int byteIndex, int byteCount)
        {
            RequireArray(bytes, "bytes");
            RequireNonNegative(byteIndex, "byteIndex", byteCount, "byteCount");
            RequireRangeWithin(bytes, byteIndex, byteCount);
            char[] chars = new char[byteCount];
            Decode(bytes, byteIndex, byteCount, chars, 0);
            return new string(chars);
        }

        private static void Decode(byte[] bytes, int index, int count, char[] chars, int charIndex)
        {
            for (int i = 0; i < count; i++)
            {
                int b = bytes[index + i];
                chars[charIndex + i] = b <= 0x7F ? (char)b : '?';
            }
        }
    }
}
