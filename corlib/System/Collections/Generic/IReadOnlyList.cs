// Lamella managed corlib (from scratch). -- System.Collections.Generic.IReadOnlyList<T>
#if LAMELLA_SURFACE_NETFX_4_5
namespace System.Collections.Generic
{

    /// <summary>A read-only collection of <typeparamref name="T"/> whose elements are read by position.</summary>
    /// <typeparam name="T">The type of the elements.</typeparam>
    public interface IReadOnlyList<T> : IReadOnlyCollection<T>
    {
        /// <summary>The element at <paramref name="index"/>.</summary>
        /// <param name="index">The zero-based position of the element to read.</param>
        T this[int index] { get; }
    }
}
#endif
