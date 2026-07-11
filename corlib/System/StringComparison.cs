// Lamella managed corlib (from scratch). -- System.StringComparison
#if LAMELLA_SURFACE_STRING_COMPARISON
namespace System
{
    public enum StringComparison
    {
        CurrentCulture = 0,
        CurrentCultureIgnoreCase = 1,
        InvariantCulture = 2,
        InvariantCultureIgnoreCase = 3,
        Ordinal = 4,
        OrdinalIgnoreCase = 5
    }
}
#endif
