// Lamella managed corlib (from scratch). -- System.Threading.ThreadPriority
#if LAMELLA_SURFACE_THREADS
namespace System.Threading
{
    public enum ThreadPriority
    {
        Lowest = 0,
        BelowNormal = 1,
        Normal = 2,
        AboveNormal = 3,
        Highest = 4
    }
}
#endif
