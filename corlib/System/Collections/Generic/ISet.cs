// Lamella managed corlib (from scratch). -- System.Collections.Generic.ISet<T>
#if LAMELLA_SURFACE_NETFX_4_0
namespace System.Collections.Generic
{
    /// <summary>A collection of distinct elements with the operations of a mathematical set.</summary>
    /// <typeparam name="T">The type of the elements.</typeparam>
    public interface ISet<T> : ICollection<T>
    {
        /// <summary>Adds <paramref name="item"/> unless an equal element is already present.</summary>
        /// <param name="item">The element to add.</param>
        /// <returns>True when the element was added.</returns>
        new bool Add(T item);

        /// <summary>Removes every element that <paramref name="other"/> holds.</summary>
        /// <param name="other">The elements to remove.</param>
        void ExceptWith(IEnumerable<T> other);

        /// <summary>Keeps only the elements that <paramref name="other"/> also holds.</summary>
        /// <param name="other">The elements to keep.</param>
        void IntersectWith(IEnumerable<T> other);

        /// <summary>Whether every element of this set is in <paramref name="other"/>, which holds at least one more.</summary>
        /// <param name="other">The collection to compare with.</param>
        /// <returns>True when this set is a proper subset of <paramref name="other"/>.</returns>
        bool IsProperSubsetOf(IEnumerable<T> other);

        /// <summary>Whether every element of <paramref name="other"/> is in this set, which holds at least one more.</summary>
        /// <param name="other">The collection to compare with.</param>
        /// <returns>True when this set is a proper superset of <paramref name="other"/>.</returns>
        bool IsProperSupersetOf(IEnumerable<T> other);

        /// <summary>Whether every element of this set is in <paramref name="other"/>.</summary>
        /// <param name="other">The collection to compare with.</param>
        /// <returns>True when this set is a subset of <paramref name="other"/>.</returns>
        bool IsSubsetOf(IEnumerable<T> other);

        /// <summary>Whether every element of <paramref name="other"/> is in this set.</summary>
        /// <param name="other">The collection to compare with.</param>
        /// <returns>True when this set is a superset of <paramref name="other"/>.</returns>
        bool IsSupersetOf(IEnumerable<T> other);

        /// <summary>Whether this set and <paramref name="other"/> share at least one element.</summary>
        /// <param name="other">The collection to compare with.</param>
        /// <returns>True when the two overlap.</returns>
        bool Overlaps(IEnumerable<T> other);

        /// <summary>Whether this set and <paramref name="other"/> hold the same elements, ignoring order and duplicates.</summary>
        /// <param name="other">The collection to compare with.</param>
        /// <returns>True when the two are equal as sets.</returns>
        bool SetEquals(IEnumerable<T> other);

        /// <summary>Keeps the elements that are in this set or in <paramref name="other"/>, but not in both.</summary>
        /// <param name="other">The collection to combine with.</param>
        void SymmetricExceptWith(IEnumerable<T> other);

        /// <summary>Adds every element of <paramref name="other"/> that is not already present.</summary>
        /// <param name="other">The elements to add.</param>
        void UnionWith(IEnumerable<T> other);
    }
}
#endif
