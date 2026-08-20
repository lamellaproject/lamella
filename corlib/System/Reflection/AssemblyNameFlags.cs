// Lamella managed corlib (from scratch). -- System.Reflection.AssemblyNameFlags
#if LAMELLA_SURFACE_REFLECTION
namespace System.Reflection
{
    public enum AssemblyNameFlags
    {
        None = 0,
        LongevityUnspecified = 0,
        PublicKey = 1,
        Library = 2,
        AppDomainPlatform = 4,
        ProcessPlatform = 6,
        SystemPlatform = 8,
        LongevityMask = 14,
        Retargetable = 256,
        EnableJITcompileOptimizer = 16384,
        EnableJITcompileTracking = 32768
    }
}
#endif
