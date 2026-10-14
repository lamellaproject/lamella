// Lamella managed corlib (from scratch). -- System.Activator
#if LAMELLA_SURFACE_REFLECTION
namespace System
{
#if LAMELLA_SURFACE_NETCORE_2_0
    public static class Activator
#else
    public sealed class Activator
#endif
    {
#if !LAMELLA_SURFACE_NETCORE_2_0
        private Activator() { }
#endif

        [Lamella.Runtime.RuntimeProvided] public static object CreateInstance(Type type) { return null; }
    }
}
#endif
