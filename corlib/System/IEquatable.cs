// Lamella managed corlib (from scratch). -- System.IEquatable<T>
#if LAMELLA_SURFACE_NETFX_2_0
namespace System
{
    /// <summary>Defines a type-specific equality test, so a value can be compared without boxing.</summary>
    /// <typeparam name="T">The type of object this instance compares against.</typeparam>
    public interface IEquatable<T>
    {
        /// <summary>Whether this instance equals <paramref name="other"/>.</summary>
        /// <param name="other">The value to compare against.</param>
        /// <returns>True when the two are equal.</returns>
        bool Equals(T other);
    }
}
#endif
