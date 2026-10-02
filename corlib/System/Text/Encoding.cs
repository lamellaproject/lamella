// Lamella managed corlib (from scratch). -- System.Text.Encoding
namespace System.Text
{
    /// <summary>Converts between strings and the bytes that encode them.</summary>
    public abstract class Encoding
    {
        private static UTF8Encoding _utf8;
        private static ASCIIEncoding _ascii;
        private static UnicodeEncoding _unicode;

        static Encoding()
        {
            _utf8 = new UTF8Encoding();
            _ascii = new ASCIIEncoding();
            _unicode = new UnicodeEncoding();
        }

        public static Encoding UTF8 { get { return _utf8; } }

        public static Encoding ASCII { get { return _ascii; } }
        public static Encoding Unicode { get { return _unicode; } }

        public abstract byte[] GetBytes(string s);
        public abstract int GetByteCount(string s);

        /// <summary>Decodes every byte of an array into a string.</summary>
        /// <param name="bytes">The bytes to decode.</param>
        /// <returns>The decoded text.</returns>
        /// <exception cref="ArgumentNullException"><paramref name="bytes"/> is null.</exception>
        public virtual string GetString(byte[] bytes)
        {
            if (bytes == null) throw new ArgumentNullException("bytes");
            return GetString(bytes, 0, bytes.Length);
        }

        /// <summary>Decodes a range of bytes into a string.</summary>
        /// <param name="bytes">The array holding the bytes to decode.</param>
        /// <param name="index">The position of the first byte to decode.</param>
        /// <param name="count">How many bytes to decode.</param>
        /// <returns>The decoded text.</returns>
        /// <exception cref="ArgumentNullException"><paramref name="bytes"/> is null.</exception>
        /// <exception cref="ArgumentOutOfRangeException"><paramref name="index"/> or
        /// <paramref name="count"/> is negative, or the range does not lie within
        /// <paramref name="bytes"/>.</exception>
        public abstract string GetString(byte[] bytes, int index, int count);

        internal const char ReplacementCharacter = '\uFFFD';

        internal static void RequireRangeWithin(byte[] bytes, int index, int count)
        {
            if (bytes.Length - index < count)
            {
                throw new ArgumentOutOfRangeException("bytes", "Index and count must refer to a location within the buffer.");
            }
        }
    }
}
