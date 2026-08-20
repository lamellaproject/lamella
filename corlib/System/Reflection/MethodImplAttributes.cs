// Lamella managed corlib (from scratch). -- System.Reflection.MethodImplAttributes
#if LAMELLA_SURFACE_REFLECTION
namespace System.Reflection
{
    public enum MethodImplAttributes
    {
        IL = 0,
        Managed = 0,
        Native = 1,
        OPTIL = 2,
        Runtime = 3,
        CodeTypeMask = 3,
        Unmanaged = 4,
        ManagedMask = 4,
        NoInlining = 8,
        ForwardRef = 16,
        Synchronized = 32,
        PreserveSig = 128,
        InternalCall = 4096,
        MaxMethodImplVal = 65535
    }
}
#endif
