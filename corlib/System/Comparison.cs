// Lamella managed corlib (from scratch). -- System.Comparison<T>
#if LAMELLA_SURFACE_NETFX_2_0
namespace System
{
    /// <summary>A method that compares two values of the same type.</summary>
    /// <typeparam name="T">The type of the values compared.</typeparam>
    /// <param name="x">The first value.</param>
    /// <param name="y">The second value.</param>
    /// <returns>Less than zero when <paramref name="x"/> sorts before <paramref name="y"/>, zero when
    /// the two sort together, and greater than zero when it sorts after.</returns>
    public delegate int Comparison<T>(T x, T y);
}
#endif
