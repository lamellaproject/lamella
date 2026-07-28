// Lamella managed corlib (from scratch). -- System.GC
#if LAMELLA_SURFACE_GC
namespace System
{
    public sealed class GC
    {
        private GC() { }

        [Lamella.Runtime.RuntimeProvided]
        [Lamella.Runtime.IntendedDefault]
        public static void Collect() { }

#if LAMELLA_SURFACE_NETFX_1_1
        public static void Collect(int generation)
        {
            if (generation < 0) throw new ArgumentOutOfRangeException("generation");
            Collect();
        }
#endif

        public static int MaxGeneration { get { return 0; } }

        public static int GetGeneration(object obj) { return 0; }

        public static void KeepAlive(object obj) { }

        [Lamella.Runtime.RuntimeProvided]
        [Lamella.Runtime.IntendedDefault]
        public static void SuppressFinalize(object obj) { }

        [Lamella.Runtime.RuntimeProvided]
        [Lamella.Runtime.IntendedDefault]
        public static void ReRegisterForFinalize(object obj) { }

        [Lamella.Runtime.RuntimeProvided]
        [Lamella.Runtime.IntendedDefault]
        public static void WaitForPendingFinalizers() { }
    }
}
#endif
