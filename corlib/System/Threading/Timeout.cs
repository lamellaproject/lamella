// Lamella managed corlib (from scratch). -- System.Threading.Timeout
#if LAMELLA_SURFACE_THREADS
namespace System.Threading
{
    public sealed class Timeout
    {
        private Timeout() { }

        public const int Infinite = -1;
    }
}
#endif
