// Lamella managed corlib (from scratch). -- System.Predicate<T>
#if LAMELLA_SURFACE_NETFX_2_0
namespace System
{
    /// <summary>A method that decides whether an object meets a condition.</summary>
    /// <typeparam name="T">The type of the object tested.</typeparam>
    /// <param name="obj">The object to test.</param>
    /// <returns>True when the object meets the condition.</returns>
    public delegate bool Predicate<T>(T obj);
}
#endif
