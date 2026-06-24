// Lamella managed corlib (from scratch). -- System.Reflection.BindingFlags
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
    }
}
