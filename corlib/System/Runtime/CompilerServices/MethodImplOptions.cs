// Lamella managed corlib (from scratch). -- System.Runtime.CompilerServices.MethodImplOptions
namespace System.Runtime.CompilerServices
{
#if LAMELLA_SURFACE_NETFX_1_1
    [Flags]
#endif
    public enum MethodImplOptions
    {
        Unmanaged = 4,
        NoInlining = 8,
        ForwardRef = 16,
        Synchronized = 32,
#if LAMELLA_SURFACE_NETFX_2_0
        NoOptimization = 64,
#endif
        PreserveSig = 128,
#if LAMELLA_SURFACE_NETFX_4_5
        AggressiveInlining = 256,
#endif
        InternalCall = 4096
    }
}
