// Lamella managed corlib (from scratch). -- System.GC
#if LAMELLA_SURFACE_GC
namespace System
{
    public sealed class GC
    {
        private GC() { }

        [Lamella.Runtime.RuntimeProvided] public static void Collect() { }

        public static void Collect(int generation)
        {
            if (generation < 0) throw new ArgumentOutOfRangeException("generation");
            Collect();
        }

        public static int MaxGeneration { get { return 0; } }

        public static int GetGeneration(object obj) { return 0; }

        public static void KeepAlive(object obj) { }

        [Lamella.Runtime.RuntimeProvided] public static void SuppressFinalize(object obj) { }

        [Lamella.Runtime.RuntimeProvided] public static void ReRegisterForFinalize(object obj) { }

        [Lamella.Runtime.RuntimeProvided] public static void WaitForPendingFinalizers() { }
    }
}
#endif
