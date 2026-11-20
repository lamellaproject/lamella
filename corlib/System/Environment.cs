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

#if LAMELLA_SURFACE_NET_5_0
        private static OperatingSystem s_osVersion;

        /// <summary>The operating system the program runs on: <c>Other 1.0.0.0</c> on a Lamella board, which runs no
        /// operating system .NET names.</summary>
        public static OperatingSystem OSVersion
        {
            get
            {
                if ((object)s_osVersion == null) s_osVersion = new OperatingSystem(PlatformID.Other, new Version(1, 0, 0, 0));
                return s_osVersion;
            }
        }
#endif
    }
}
