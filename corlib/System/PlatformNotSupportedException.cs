// Lamella managed corlib (from scratch). -- System.PlatformNotSupportedException
#if LAMELLA_SURFACE_NETFX_2_0
namespace System
{

    /// <summary>The exception thrown when a feature does not run on the current platform.</summary>
    public class PlatformNotSupportedException : NotSupportedException
    {
        /// <summary>Initializes the exception with a message saying the operation is not supported.</summary>
        public PlatformNotSupportedException() : base("Operation is not supported on this platform.")
        {
        }

        /// <summary>Initializes the exception with <paramref name="message"/>.</summary>
        /// <param name="message">The text describing the failure.</param>
        public PlatformNotSupportedException(string message) : base(message)
        {
        }

        /// <summary>Initializes the exception with <paramref name="message"/> and the exception that caused it.</summary>
        /// <param name="message">The text describing the failure.</param>
        /// <param name="inner">The exception that caused this one.</param>
        public PlatformNotSupportedException(string message, Exception inner) : base(message, inner)
        {
        }
    }
}
#endif
