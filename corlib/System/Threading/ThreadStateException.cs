// Lamella managed corlib (from scratch). -- System.Threading.ThreadStateException
#if LAMELLA_SURFACE_THREADS
namespace System.Threading
{
    public class ThreadStateException : SystemException
    {
        public ThreadStateException() : base() { }
        public ThreadStateException(string message) : base(message) { }
        public ThreadStateException(string message, Exception innerException) : base(message, innerException) { }
    }
}
#endif
