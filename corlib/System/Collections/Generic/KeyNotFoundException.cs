// Lamella managed corlib (from scratch). -- System.Collections.Generic.KeyNotFoundException
#if LAMELLA_SURFACE_NETFX_2_0
namespace System.Collections.Generic
{

    /// <summary>The exception thrown when a key is read from a collection that does not contain it.</summary>
    public class KeyNotFoundException : SystemException
    {
        /// <summary>Initializes the exception with a message saying the key was not present.</summary>
        public KeyNotFoundException() : base("The given key was not present in the dictionary.")
        {
        }

        /// <summary>Initializes the exception with <paramref name="message"/>.</summary>
        /// <param name="message">The text describing the failure.</param>
        public KeyNotFoundException(string message) : base(message)
        {
        }

        /// <summary>Initializes the exception with <paramref name="message"/> and the exception that caused it.</summary>
        /// <param name="message">The text describing the failure.</param>
        /// <param name="innerException">The exception that caused this one.</param>
        public KeyNotFoundException(string message, Exception innerException) : base(message, innerException)
        {
        }
    }
}
#endif
