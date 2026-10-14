// Lamella managed corlib (from scratch). -- System.Threading.Interlocked
#if LAMELLA_SURFACE_THREADS
namespace System.Threading
{
#if LAMELLA_SURFACE_NETFX_2_0
    public static class Interlocked
#else
    public sealed class Interlocked
#endif
    {
#if !LAMELLA_SURFACE_NETFX_2_0
        private Interlocked() { }
#endif

        public static int Increment(ref int location) { location = location + 1; return location; }
        public static int Decrement(ref int location) { location = location - 1; return location; }
#if LAMELLA_SURFACE_NETFX_2_0
        public static int Add(ref int location, int value) { location = location + value; return location; }
#endif

        public static int Exchange(ref int location, int value)
        {
            int original = location;
            location = value;
            return original;
        }

        public static int CompareExchange(ref int location, int value, int comparand)
        {
            int original = location;
            if (original == comparand) location = value;
            return original;
        }

#if LAMELLA_SURFACE_NETFX_2_0
        public static long Increment(ref long location) { location = location + 1; return location; }
        public static long Decrement(ref long location) { location = location - 1; return location; }
        public static long Add(ref long location, long value) { location = location + value; return location; }

        public static long Exchange(ref long location, long value)
        {
            long original = location;
            location = value;
            return original;
        }

        public static long CompareExchange(ref long location, long value, long comparand)
        {
            long original = location;
            if (original == comparand) location = value;
            return original;
        }
#endif
    }
}
#endif
