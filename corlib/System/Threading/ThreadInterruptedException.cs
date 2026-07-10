// Lamella managed corlib (from scratch). -- System.Threading.ThreadInterruptedException
#if LAMELLA_SURFACE_THREADS
namespace System.Threading
{
    public class ThreadInterruptedException : SystemException
    {
        public ThreadInterruptedException() : base() { }
        public ThreadInterruptedException(string message) : base(message) { }
        public ThreadInterruptedException(string message, Exception innerException) : base(message, innerException) { }
    }
}
#endif
