// Lamella managed corlib (from scratch). -- System.Reflection.BindingFlags
#if LAMELLA_SURFACE_REFLECTION
namespace System.Reflection
{
    public enum BindingFlags
    {
        Default = 0,
        IgnoreCase = 1,
        DeclaredOnly = 2,
        Instance = 4,
        Static = 8,
        Public = 16,
        NonPublic = 32,
        FlattenHierarchy = 64,

        InvokeMethod = 0x100,
        CreateInstance = 0x200,
        GetField = 0x400,
        SetField = 0x800,
        GetProperty = 0x1000,
        SetProperty = 0x2000,
        PutDispProperty = 0x4000,
        PutRefDispProperty = 0x8000,
        ExactBinding = 0x10000,
        SuppressChangeType = 0x20000,
        OptionalParamBinding = 0x40000,
        IgnoreReturn = 0x1000000,
    }
}
#endif
