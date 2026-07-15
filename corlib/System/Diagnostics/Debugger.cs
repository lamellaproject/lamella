// Lamella managed corlib (from scratch). -- System.Diagnostics.Debugger
namespace System.Diagnostics
{
    public sealed class Debugger
    {
        private Debugger() { }

        public const string DefaultCategory = "";

        public static void Break() { }

        public static bool Launch() { return false; }

        public static bool IsLogging() { return false; }

        public static void Log(int level, string category, string message) { }

#if LAMELLA_NET_2_0
        public static bool IsAttached { get { return false; } }
#endif
    }
}
