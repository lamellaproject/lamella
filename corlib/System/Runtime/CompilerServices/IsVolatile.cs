// Lamella managed corlib (from scratch). -- System.Runtime.CompilerServices.IsVolatile
namespace System.Runtime.CompilerServices
{
    /// <summary>Marks a location in the memory store as volatile. A compiler names it as a required modifier on the
    /// type of a volatile field.</summary>
#if LAMELLA_SURFACE_NETFX_2_0
    public static class IsVolatile
#else
    public sealed class IsVolatile
#endif
    {
#if !LAMELLA_SURFACE_NETFX_2_0
        private IsVolatile() { }
#endif
    }
}
