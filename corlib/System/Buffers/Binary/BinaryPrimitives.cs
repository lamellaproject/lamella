// Lamella managed corlib (from scratch). -- System.Buffers.Binary.BinaryPrimitives
#if LAMELLA_SURFACE_NETCORE_2_1 && LAMELLA_SURFACE_SPAN
namespace System.Buffers.Binary
{
    /// <summary>Reads and writes integers in an explicit byte order, whatever the order of the machine the
    /// program runs on.</summary>
    public static class BinaryPrimitives
    {
        private static void RequireLength(int length, int width)
        {
            if (length < width) throw new ArgumentOutOfRangeException("length");
        }

        /// <summary>Reads an unsigned 16-bit integer from the first two bytes of a span, most significant
        /// byte first.</summary>
        /// <param name="source">The bytes to read. Any bytes after the first two are ignored.</param>
        /// <returns>The integer the two bytes encode.</returns>
        /// <exception cref="ArgumentOutOfRangeException"><paramref name="source"/> holds fewer than two
        /// bytes.</exception>
        public static ushort ReadUInt16BigEndian(ReadOnlySpan<byte> source)
        {
            RequireLength(source.Length, 2);
            return (ushort)((source[0] << 8) | source[1]);
        }

        /// <summary>Reads a signed 16-bit integer from the first two bytes of a span, most significant byte
        /// first.</summary>
        /// <param name="source">The bytes to read. Any bytes after the first two are ignored.</param>
        /// <returns>The integer the two bytes encode, in two's complement.</returns>
        /// <exception cref="ArgumentOutOfRangeException"><paramref name="source"/> holds fewer than two
        /// bytes.</exception>
        public static short ReadInt16BigEndian(ReadOnlySpan<byte> source)
        {
            return unchecked((short)ReadUInt16BigEndian(source));
        }

        /// <summary>Writes an unsigned 16-bit integer to the first two bytes of a span, most significant
        /// byte first.</summary>
        /// <param name="destination">The bytes to write. Any bytes after the first two are left
        /// unchanged.</param>
        /// <param name="value">The integer to write.</param>
        /// <exception cref="ArgumentOutOfRangeException"><paramref name="destination"/> holds fewer than two
        /// bytes. Nothing is written.</exception>
        public static void WriteUInt16BigEndian(Span<byte> destination, ushort value)
        {
            RequireLength(destination.Length, 2);
            destination[0] = (byte)(value >> 8);
            destination[1] = (byte)value;
        }

        /// <summary>Writes a signed 16-bit integer to the first two bytes of a span, most significant byte
        /// first.</summary>
        /// <param name="destination">The bytes to write. Any bytes after the first two are left
        /// unchanged.</param>
        /// <param name="value">The integer to write, in two's complement.</param>
        /// <exception cref="ArgumentOutOfRangeException"><paramref name="destination"/> holds fewer than two
        /// bytes. Nothing is written.</exception>
        public static void WriteInt16BigEndian(Span<byte> destination, short value)
        {
            WriteUInt16BigEndian(destination, unchecked((ushort)value));
        }

        /// <summary>Reads an unsigned 16-bit integer from the first two bytes of a span, least significant
        /// byte first.</summary>
        /// <param name="source">The bytes to read. Any bytes after the first two are ignored.</param>
        /// <returns>The integer the two bytes encode.</returns>
        /// <exception cref="ArgumentOutOfRangeException"><paramref name="source"/> holds fewer than two
        /// bytes.</exception>
        public static ushort ReadUInt16LittleEndian(ReadOnlySpan<byte> source)
        {
            RequireLength(source.Length, 2);
            return (ushort)(source[0] | (source[1] << 8));
        }

        /// <summary>Reads a signed 16-bit integer from the first two bytes of a span, least significant
        /// byte first.</summary>
        /// <param name="source">The bytes to read. Any bytes after the first two are ignored.</param>
        /// <returns>The integer the two bytes encode, in two's complement.</returns>
        /// <exception cref="ArgumentOutOfRangeException"><paramref name="source"/> holds fewer than two
        /// bytes.</exception>
        public static short ReadInt16LittleEndian(ReadOnlySpan<byte> source)
        {
            return unchecked((short)ReadUInt16LittleEndian(source));
        }

        /// <summary>Writes an unsigned 16-bit integer to the first two bytes of a span, least significant
        /// byte first.</summary>
        /// <param name="destination">The bytes to write. Any bytes after the first two are left
        /// unchanged.</param>
        /// <param name="value">The integer to write.</param>
        /// <exception cref="ArgumentOutOfRangeException"><paramref name="destination"/> holds fewer than two
        /// bytes. Nothing is written.</exception>
        public static void WriteUInt16LittleEndian(Span<byte> destination, ushort value)
        {
            RequireLength(destination.Length, 2);
            destination[0] = (byte)value;
            destination[1] = (byte)(value >> 8);
        }

        /// <summary>Writes a signed 16-bit integer to the first two bytes of a span, least significant byte
        /// first.</summary>
        /// <param name="destination">The bytes to write. Any bytes after the first two are left
        /// unchanged.</param>
        /// <param name="value">The integer to write, in two's complement.</param>
        /// <exception cref="ArgumentOutOfRangeException"><paramref name="destination"/> holds fewer than two
        /// bytes. Nothing is written.</exception>
        public static void WriteInt16LittleEndian(Span<byte> destination, short value)
        {
            WriteUInt16LittleEndian(destination, unchecked((ushort)value));
        }

        /// <summary>Reads an unsigned 32-bit integer from the first four bytes of a span, most significant
        /// byte first.</summary>
        /// <param name="source">The bytes to read. Any bytes after the first four are ignored.</param>
        /// <returns>The integer the four bytes encode.</returns>
        /// <exception cref="ArgumentOutOfRangeException"><paramref name="source"/> holds fewer than four
        /// bytes.</exception>
        public static uint ReadUInt32BigEndian(ReadOnlySpan<byte> source)
        {
            RequireLength(source.Length, 4);
            return ((uint)source[0] << 24) | ((uint)source[1] << 16) | ((uint)source[2] << 8) | source[3];
        }

        /// <summary>Reads a signed 32-bit integer from the first four bytes of a span, most significant
        /// byte first.</summary>
        /// <param name="source">The bytes to read. Any bytes after the first four are ignored.</param>
        /// <returns>The integer the four bytes encode, in two's complement.</returns>
        /// <exception cref="ArgumentOutOfRangeException"><paramref name="source"/> holds fewer than four
        /// bytes.</exception>
        public static int ReadInt32BigEndian(ReadOnlySpan<byte> source)
        {
            return unchecked((int)ReadUInt32BigEndian(source));
        }

        /// <summary>Writes an unsigned 32-bit integer to the first four bytes of a span, most significant
        /// byte first.</summary>
        /// <param name="destination">The bytes to write. Any bytes after the first four are left
        /// unchanged.</param>
        /// <param name="value">The integer to write.</param>
        /// <exception cref="ArgumentOutOfRangeException"><paramref name="destination"/> holds fewer than
        /// four bytes. Nothing is written.</exception>
        public static void WriteUInt32BigEndian(Span<byte> destination, uint value)
        {
            RequireLength(destination.Length, 4);
            destination[0] = (byte)(value >> 24);
            destination[1] = (byte)(value >> 16);
            destination[2] = (byte)(value >> 8);
            destination[3] = (byte)value;
        }

        /// <summary>Writes a signed 32-bit integer to the first four bytes of a span, most significant byte
        /// first.</summary>
        /// <param name="destination">The bytes to write. Any bytes after the first four are left
        /// unchanged.</param>
        /// <param name="value">The integer to write, in two's complement.</param>
        /// <exception cref="ArgumentOutOfRangeException"><paramref name="destination"/> holds fewer than
        /// four bytes. Nothing is written.</exception>
        public static void WriteInt32BigEndian(Span<byte> destination, int value)
        {
            WriteUInt32BigEndian(destination, unchecked((uint)value));
        }

        /// <summary>Reads an unsigned 32-bit integer from the first four bytes of a span, least significant
        /// byte first.</summary>
        /// <param name="source">The bytes to read. Any bytes after the first four are ignored.</param>
        /// <returns>The integer the four bytes encode.</returns>
        /// <exception cref="ArgumentOutOfRangeException"><paramref name="source"/> holds fewer than four
        /// bytes.</exception>
        public static uint ReadUInt32LittleEndian(ReadOnlySpan<byte> source)
        {
            RequireLength(source.Length, 4);
            return source[0] | ((uint)source[1] << 8) | ((uint)source[2] << 16) | ((uint)source[3] << 24);
        }

        /// <summary>Reads a signed 32-bit integer from the first four bytes of a span, least significant
        /// byte first.</summary>
        /// <param name="source">The bytes to read. Any bytes after the first four are ignored.</param>
        /// <returns>The integer the four bytes encode, in two's complement.</returns>
        /// <exception cref="ArgumentOutOfRangeException"><paramref name="source"/> holds fewer than four
        /// bytes.</exception>
        public static int ReadInt32LittleEndian(ReadOnlySpan<byte> source)
        {
            return unchecked((int)ReadUInt32LittleEndian(source));
        }

        /// <summary>Writes an unsigned 32-bit integer to the first four bytes of a span, least significant
        /// byte first.</summary>
        /// <param name="destination">The bytes to write. Any bytes after the first four are left
        /// unchanged.</param>
        /// <param name="value">The integer to write.</param>
        /// <exception cref="ArgumentOutOfRangeException"><paramref name="destination"/> holds fewer than
        /// four bytes. Nothing is written.</exception>
        public static void WriteUInt32LittleEndian(Span<byte> destination, uint value)
        {
            RequireLength(destination.Length, 4);
            destination[0] = (byte)value;
            destination[1] = (byte)(value >> 8);
            destination[2] = (byte)(value >> 16);
            destination[3] = (byte)(value >> 24);
        }

        /// <summary>Writes a signed 32-bit integer to the first four bytes of a span, least significant
        /// byte first.</summary>
        /// <param name="destination">The bytes to write. Any bytes after the first four are left
        /// unchanged.</param>
        /// <param name="value">The integer to write, in two's complement.</param>
        /// <exception cref="ArgumentOutOfRangeException"><paramref name="destination"/> holds fewer than
        /// four bytes. Nothing is written.</exception>
        public static void WriteInt32LittleEndian(Span<byte> destination, int value)
        {
            WriteUInt32LittleEndian(destination, unchecked((uint)value));
        }

        /// <summary>Writes an unsigned 32-bit integer to the first four bytes of a span, most significant
        /// byte first, if the span is long enough.</summary>
        /// <param name="destination">The bytes to write. Any bytes after the first four are left
        /// unchanged.</param>
        /// <param name="value">The integer to write.</param>
        /// <returns>True when the integer was written; false when <paramref name="destination"/> holds
        /// fewer than four bytes, in which case nothing is written.</returns>
        public static bool TryWriteUInt32BigEndian(Span<byte> destination, uint value)
        {
            if (destination.Length < 4) return false;
            WriteUInt32BigEndian(destination, value);
            return true;
        }
    }
}
#endif
