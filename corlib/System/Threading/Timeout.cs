// Lamella managed corlib (from scratch). -- System.Threading.Timeout
#if LAMELLA_SURFACE_THREADS
namespace System.Threading
{
#if LAMELLA_SURFACE_NETFX_2_0
    public static class Timeout
#else
    public sealed class Timeout
#endif
    {
#if !LAMELLA_SURFACE_NETFX_2_0
        private Timeout() { }
#endif

        public const int Infinite = -1;
    }
}
#endif
