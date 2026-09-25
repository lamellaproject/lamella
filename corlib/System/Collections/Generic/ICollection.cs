// Lamella managed corlib (from scratch). -- System.Collections.Generic.ICollection<T>
#if LAMELLA_SURFACE_NETFX_2_0
namespace System.Collections.Generic
{

    /// <summary>A sized collection of <typeparamref name="T"/> that can be added to, searched and emptied.</summary>
    /// <typeparam name="T">The type of the elements.</typeparam>
    public interface ICollection<T> : IEnumerable<T>
    {
        /// <summary>How many elements the collection holds.</summary>
        int Count { get; }

        /// <summary>Whether the collection refuses to be modified.</summary>
        bool IsReadOnly { get; }

        /// <summary>Adds <paramref name="item"/> to the collection.</summary>
        /// <param name="item">The element to add.</param>
        void Add(T item);

        /// <summary>Removes every element.</summary>
        void Clear();

        /// <summary>Whether the collection holds an element equal to <paramref name="item"/>.</summary>
        /// <param name="item">The element to look for.</param>
        /// <returns>True when a matching element is present.</returns>
        bool Contains(T item);

        /// <summary>Copies the elements into <paramref name="array"/>, starting at <paramref name="arrayIndex"/>.</summary>
        /// <param name="array">The destination array.</param>
        /// <param name="arrayIndex">The first index of <paramref name="array"/> written.</param>
        void CopyTo(T[] array, int arrayIndex);

        /// <summary>Removes the first element equal to <paramref name="item"/>.</summary>
        /// <param name="item">The element to remove.</param>
        /// <returns>True when an element was removed.</returns>
        bool Remove(T item);
    }
}
#endif
