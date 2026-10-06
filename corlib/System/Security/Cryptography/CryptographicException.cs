// Lamella managed corlib (from scratch). -- System.Security.Cryptography.CryptographicException
#if LAMELLA_SURFACE_NET_TLS
namespace System.Security.Cryptography
{

    /// <summary>The exception a cryptographic operation throws when it fails: a certificate or key that cannot be read, or a key that does not belong to its certificate.</summary>
    public class CryptographicException : SystemException
    {
        private const string DefaultMessage = "Error occurred during a cryptographic operation.";

        /// <summary>Initializes the exception with the default message.</summary>
        public CryptographicException() : base(DefaultMessage) { }

        /// <summary>Initializes the exception for an error code, with the default message.</summary>
        public CryptographicException(int hr) : base(DefaultMessage) { }

        /// <summary>Initializes the exception with a message.</summary>
        public CryptographicException(string message) : base(message) { }

        /// <summary>Initializes the exception with a message and the exception that caused it.</summary>
        public CryptographicException(string message, Exception inner) : base(message, inner) { }

        /// <summary>Initializes the exception with a message formatted from <paramref name="format"/> and <paramref name="insert"/>.</summary>
        public CryptographicException(string format, string insert) : base(String.Format(format, insert)) { }
    }
}
#endif
