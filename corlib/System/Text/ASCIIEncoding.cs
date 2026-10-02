// Lamella managed corlib (from scratch). -- System.Text.ASCIIEncoding
namespace System.Text
{
    public class ASCIIEncoding : Encoding
    {
        public ASCIIEncoding() { }

        public override int GetByteCount(string s) { return s.Length; }

        public override byte[] GetBytes(string s)
        {
            byte[] bytes = new byte[s.Length];
            for (int i = 0; i < s.Length; i++)
            {
                int c = s[i];
                bytes[i] = (byte)(c <= 0x7F ? c : '?');
            }
            return bytes;
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
            if (bytes == null) throw new ArgumentNullException("bytes", "Array cannot be null.");
            if (byteIndex < 0) throw new ArgumentOutOfRangeException("byteIndex", "Non-negative number required.");
            if (byteCount < 0) throw new ArgumentOutOfRangeException("byteCount", "Non-negative number required.");
            RequireRangeWithin(bytes, byteIndex, byteCount);

            StringBuilder result = new StringBuilder(byteCount);
            int end = byteIndex + byteCount;
            for (int i = byteIndex; i < end; i++)
            {
                int b = bytes[i];
                result.Append(b <= 0x7F ? (char)b : '?');
            }
            return result.ToString();
        }
    }
}
