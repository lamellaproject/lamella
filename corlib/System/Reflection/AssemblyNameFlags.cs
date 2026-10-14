// Lamella managed corlib (from scratch). -- System.Reflection.AssemblyNameFlags
#if LAMELLA_SURFACE_REFLECTION
namespace System.Reflection
{
    public enum AssemblyNameFlags
    {
        None = 0,
        PublicKey = 1,
#if LAMELLA_SURFACE_NETFX_1_1
        Retargetable = 256,
#endif
#if LAMELLA_SURFACE_NETFX_2_0
        EnableJITcompileOptimizer = 16384,
        EnableJITcompileTracking = 32768,
#endif
    }
}
#endif
