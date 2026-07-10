// Lamella managed corlib (from scratch). -- System.Threading.SynchronizationLockException
#if LAMELLA_SURFACE_THREADS
namespace System.Threading
{
    public class SynchronizationLockException : SystemException
    {
        public SynchronizationLockException() : base() { }
        public SynchronizationLockException(string message) : base(message) { }
        public SynchronizationLockException(string message, Exception innerException) : base(message, innerException) { }
    }
}
#endif
