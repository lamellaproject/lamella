// Lamella managed corlib (from scratch). -- System.Activator
#if LAMELLA_SURFACE_REFLECTION
namespace System
{
    public sealed class Activator
    {
        [Lamella.Runtime.RuntimeProvided] public static object CreateInstance(Type type) { return null; }
    }
}
#endif
