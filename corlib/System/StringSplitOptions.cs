// Lamella managed corlib (from scratch). -- System.StringSplitOptions
#if LAMELLA_SURFACE_NETFX_2_0
namespace System
{
    [Flags]
    public enum StringSplitOptions
    {
        None = 0,
        RemoveEmptyEntries = 1,
#if LAMELLA_SURFACE_NET_5_0
        TrimEntries = 2
#endif
    }
}
#endif
