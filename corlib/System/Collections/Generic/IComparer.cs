// Lamella managed corlib (from scratch). -- System.Collections.Generic.IComparer<T>
#if LAMELLA_SURFACE_NETFX_2_0
namespace System.Collections.Generic
{
    /// <summary>Defines a method that compares two values of a type.</summary>
    /// <typeparam name="T">The type of the values compared.</typeparam>
    public interface IComparer<T>
    {
        /// <summary>Compares two values.</summary>
        /// <param name="x">The first value.</param>
        /// <param name="y">The second value.</param>
        /// <returns>Less than zero when <paramref name="x"/> sorts before <paramref name="y"/>, zero
        /// when the two sort together, and greater than zero when it sorts after.</returns>
        int Compare(T x, T y);
    }
}
#endif
