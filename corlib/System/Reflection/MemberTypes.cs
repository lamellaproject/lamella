// Lamella managed corlib (from scratch). -- System.Reflection.MemberTypes
#if LAMELLA_SURFACE_REFLECTION
namespace System.Reflection
{
    public enum MemberTypes
    {
        Constructor = 1,
        Event = 2,
        Field = 4,
        Method = 8,
        Property = 16,
        TypeInfo = 32,
        Custom = 64,
        NestedType = 128,
        All = 191
    }
}
#endif
