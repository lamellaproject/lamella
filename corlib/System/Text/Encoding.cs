// Lamella managed corlib (from scratch). -- System.Text.Encoding
namespace System.Text
{
    /// <summary>Converts between strings and the bytes that encode them.</summary>
    public abstract class Encoding
    {
        public static Encoding UTF8 { get { return UTF8Encoding._default; } }

        public static Encoding ASCII { get { return ASCIIEncoding._default; } }
        public static Encoding Unicode { get { return UnicodeEncoding._default; } }


        /// <summary>Counts the bytes that encoding a range of characters produces.</summary>
        /// <param name="chars">The array holding the characters to encode.</param>
        /// <param name="index">The position of the first character to encode.</param>
        /// <param name="count">How many characters to encode.</param>
        /// <returns>The number of bytes.</returns>
        public abstract int GetByteCount(char[] chars, int index, int count);

        /// <summary>Encodes a range of characters into a byte array.</summary>
        /// <param name="chars">The array holding the characters to encode.</param>
        /// <param name="charIndex">The position of the first character to encode.</param>
        /// <param name="charCount">How many characters to encode.</param>
        /// <param name="bytes">The array the bytes are written into.</param>
        /// <param name="byteIndex">The position in <paramref name="bytes"/> the first byte is written at.</param>
        /// <returns>The number of bytes written.</returns>
        public abstract int GetBytes(char[] chars, int charIndex, int charCount, byte[] bytes, int byteIndex);

        /// <summary>Counts the characters that decoding a range of bytes produces.</summary>
        /// <param name="bytes">The array holding the bytes to decode.</param>
        /// <param name="index">The position of the first byte to decode.</param>
        /// <param name="count">How many bytes to decode.</param>
        /// <returns>The number of characters.</returns>
        public abstract int GetCharCount(byte[] bytes, int index, int count);

        /// <summary>Decodes a range of bytes into a character array.</summary>
        /// <param name="bytes">The array holding the bytes to decode.</param>
        /// <param name="byteIndex">The position of the first byte to decode.</param>
        /// <param name="byteCount">How many bytes to decode.</param>
        /// <param name="chars">The array the characters are written into.</param>
        /// <param name="charIndex">The position in <paramref name="chars"/> the first character is written at.</param>
        /// <returns>The number of characters written.</returns>
        public abstract int GetChars(byte[] bytes, int byteIndex, int byteCount, char[] chars, int charIndex);

        /// <summary>Counts the bytes that encoding a string produces.</summary>
        /// <param name="s">The string to encode.</param>
        /// <returns>The number of bytes.</returns>
        /// <exception cref="ArgumentNullException"><paramref name="s"/> is null.</exception>
        public virtual int GetByteCount(string s)
        {
            if (s == null) throw new ArgumentNullException("s");
            char[] chars = s.ToCharArray();
            return GetByteCount(chars, 0, chars.Length);
        }

        /// <summary>Encodes a string into a new byte array.</summary>
        /// <param name="s">The string to encode.</param>
        /// <returns>The encoded bytes.</returns>
        /// <exception cref="ArgumentNullException"><paramref name="s"/> is null.</exception>
        public virtual byte[] GetBytes(string s)
        {
            if (s == null) throw new ArgumentNullException("s");
            byte[] bytes = new byte[GetByteCount(s)];
            char[] chars = s.ToCharArray();
            GetBytes(chars, 0, chars.Length, bytes, 0);
            return bytes;
        }

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
        public virtual string GetString(byte[] bytes, int index, int count)
        {
            char[] chars = new char[GetCharCount(bytes, index, count)];
            GetChars(bytes, index, count, chars, 0);
            return new string(chars);
        }

#if LAMELLA_SURFACE_NETCORE_2_1 && LAMELLA_SURFACE_SPAN
        /// <summary>Decodes every byte of a span into a string.</summary>
        /// <param name="bytes">The bytes to decode.</param>
        /// <returns>The decoded text, which is empty when the span is.</returns>
        public string GetString(ReadOnlySpan<byte> bytes)
        {
            if (bytes.Length == 0) return "";
            return GetString(bytes.BackingArray, bytes.BackingStart, bytes.Length);
        }
#endif

        internal const char ReplacementCharacter = '\uFFFD';


        internal static void RequireArray(object array, string name)
        {
            if (array == null) throw new ArgumentNullException(name, "Array cannot be null.");
        }

        internal static void RequireNonNegative(int index, string indexName, int count, string countName)
        {
            if (index < 0) throw new ArgumentOutOfRangeException(indexName, "Non-negative number required.");
            if (count < 0) throw new ArgumentOutOfRangeException(countName, "Non-negative number required.");
        }

        internal static void RequireRangeWithin(byte[] bytes, int index, int count)
        {
            if (bytes.Length - index < count)
            {
                throw new ArgumentOutOfRangeException("bytes", "Index and count must refer to a location within the buffer.");
            }
        }

        internal static void RequireRangeWithin(char[] chars, int index, int count, string within)
        {
            if (chars.Length - index < count)
            {
                throw new ArgumentOutOfRangeException("chars", "Index and count must refer to a location within the " + within + ".");
            }
        }

        internal static void RequireDestinationIndex(int index, int length, string name)
        {
            if (index < 0 || index > length)
            {
                throw new ArgumentOutOfRangeException(name, "Index was out of range. Must be non-negative and less than or equal to the size of the collection.");
            }
        }

        internal static void RequireByteRoom(int needed, byte[] bytes, int byteIndex, string codePage)
        {
            if (bytes.Length - byteIndex < needed)
            {
                throw new ArgumentException("The output byte buffer is too small to contain the encoded data, encoding codepage '" + codePage + "' and fallback 'System.Text.EncoderReplacementFallback'.", "bytes");
            }
        }

        internal static void RequireCharRoom(int needed, char[] chars, int charIndex, string codePage)
        {
            if (chars.Length - charIndex < needed)
            {
                throw new ArgumentException("The output char buffer is too small to contain the decoded characters, encoding codepage '" + codePage + "' and fallback 'System.Text.DecoderReplacementFallback'.", "chars");
            }
        }
    }
}
