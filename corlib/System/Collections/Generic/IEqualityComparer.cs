// Lamella managed corlib (from scratch). -- System.Collections.Generic.IEqualityComparer<T>
#if LAMELLA_SURFACE_NETFX_2_0
namespace System.Collections.Generic
{

    /// <summary>Defines how two values of <typeparamref name="T"/> are compared for equality and how one is hashed.</summary>
    /// <typeparam name="T">The type of values to compare.</typeparam>
    /// <remarks>Two values that compare equal must return the same hash code.</remarks>
    public interface IEqualityComparer<T>
    {
        /// <summary>Whether two values are equal.</summary>
        /// <param name="x">The first value.</param>
        /// <param name="y">The second value.</param>
        /// <returns>True when the two values are equal.</returns>
        bool Equals(T x, T y);

        /// <summary>A hash code for <paramref name="obj"/>.</summary>
        /// <param name="obj">The value to hash.</param>
        /// <returns>The hash code.</returns>
        int GetHashCode(T obj);
    }
}
#endif
