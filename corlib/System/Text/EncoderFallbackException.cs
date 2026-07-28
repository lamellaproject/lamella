// Lamella managed corlib (from scratch). -- System.Text.EncoderFallbackException
namespace System.Text
{

    /// <summary>The exception thrown when a character cannot be encoded into the target
    /// encoding.</summary>
    /// <remarks>
    /// <para>Deriving from <see cref="ArgumentException"/> is the .NET shape and is load-bearing
    /// here: a <c>catch (ArgumentException)</c> written against desktop .NET fires on this without
    /// naming a type that program's author never wrote.</para>
    /// <para>WHERE THIS RUNTIME RAISES IT, which is more places than .NET does. On a build whose
    /// string storage is well-formed UTF-8, <see cref="String"/> cannot hold a lone surrogate at
    /// all, so constructing one is refused -- not only <c>Encoding.GetBytes</c>. A build storing
    /// UTF-16 or WTF-8 can hold a lone surrogate and never raises it for that reason.</para>
    /// <para>DIVERGENCE, because these properties read as more informative than they are:
    /// <see cref="CharUnknown"/>, <see cref="CharUnknownHigh"/>, <see cref="CharUnknownLow"/> and
    /// <see cref="Index"/> are set only through constructors .NET keeps internal, so they are
    /// <c>'\0'</c> and <c>0</c> on any instance a program builds -- as they are in .NET. Unlike
    /// .NET, they are also left at their defaults when the RUNTIME raises this, because such an
    /// exception is not built through either constructor; it carries the offending character and
    /// its index in <c>Message</c> instead, which is the same place .NET's own message carries
    /// them. Read <c>Message</c>, not <see cref="Index"/>, for a runtime-raised one.</para>
    /// </remarks>
    public sealed class EncoderFallbackException : ArgumentException
    {
        private char _charUnknown;
        private char _charUnknownHigh;
        private char _charUnknownLow;
        private int _index;

        /// <summary>Initializes a new instance with a system-supplied message.</summary>
        public EncoderFallbackException() : base() { }

        /// <summary>Initializes a new instance with the specified message.</summary>
        public EncoderFallbackException(string message) : base(message) { }

        /// <summary>Initializes a new instance with the specified message and the exception that
        /// caused it.</summary>
        public EncoderFallbackException(string message, Exception innerException) : base(message, innerException) { }

        internal EncoderFallbackException(string message, char charUnknown, int index) : base(message)
        {
            _charUnknown = charUnknown;
            _index = index;
        }

        internal EncoderFallbackException(string message, char charUnknownHigh, char charUnknownLow, int index) : base(message)
        {
            _charUnknownHigh = charUnknownHigh;
            _charUnknownLow = charUnknownLow;
            _index = index;
        }

        /// <summary>The character that could not be encoded.</summary>
        public char CharUnknown { get { return _charUnknown; } }

        /// <summary>The high component of the surrogate pair that could not be encoded.</summary>
        public char CharUnknownHigh { get { return _charUnknownHigh; } }

        /// <summary>The low component of the surrogate pair that could not be encoded.</summary>
        public char CharUnknownLow { get { return _charUnknownLow; } }

        /// <summary>The index, in the input, of the character that could not be encoded.</summary>
        public int Index { get { return _index; } }

        /// <summary>Indicates whether the input that could not be encoded was a surrogate
        /// pair.</summary>
        /// <returns><c>true</c> when this describes a surrogate pair rather than a single
        /// character; <c>false</c> otherwise, including for any instance a program constructed
        /// itself.</returns>
        public bool IsUnknownSurrogate()
        {
            return _charUnknownHigh != '\0';
        }
    }
}
