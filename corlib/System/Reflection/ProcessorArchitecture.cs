// Lamella managed corlib (from scratch). -- System.Reflection.ProcessorArchitecture
#if LAMELLA_SURFACE_REFLECTION && LAMELLA_SURFACE_NETFX_2_0
namespace System.Reflection
{
    public enum ProcessorArchitecture
    {
        None = 0,
        MSIL = 1,
        X86 = 2,
        IA64 = 3,
        Amd64 = 4
    }
}
#endif
