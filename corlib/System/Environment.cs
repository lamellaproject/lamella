// Lamella managed corlib (from scratch). -- System.Environment
namespace System
{
#if LAMELLA_SURFACE_NETFX_2_0
    public static class Environment
#else
    public sealed class Environment
#endif
    {
#if !LAMELLA_SURFACE_NETFX_2_0
        private Environment() { }
#endif

        public static string NewLine { get { return "\r\n"; } }

        public static int TickCount
        {
            [Lamella.Runtime.RuntimeProvided] get { return 0; }
        }

#if LAMELLA_SURFACE_NETFX_2_0
        public static int ProcessorCount
        {
            [Lamella.Runtime.RuntimeProvided] get { return 0; }
        }
#endif

        [Lamella.Runtime.RuntimeProvided]
        [Lamella.Runtime.IntendedDefault]
        public static string GetEnvironmentVariable(string variable) { return null; }
    }
}
