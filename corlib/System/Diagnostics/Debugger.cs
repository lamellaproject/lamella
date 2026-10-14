// Lamella managed corlib (from scratch). -- System.Diagnostics.Debugger
namespace System.Diagnostics
{
#if LAMELLA_SURFACE_NETCORE_2_0
    public static class Debugger
#else
    public sealed class Debugger
#endif
    {
#if !LAMELLA_SURFACE_NETCORE_2_0
        private Debugger() { }
#endif

        public static readonly string DefaultCategory;

        public static void Break() { }

        public static bool Launch() { return false; }

        public static bool IsLogging() { return false; }

        public static void Log(int level, string category, string message) { }

#if LAMELLA_NET_2_0
        public static bool IsAttached { get { return false; } }
#endif
    }
}
