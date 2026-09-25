// Lamella managed corlib (from scratch). -- System.Collections.Generic.IDictionary<TKey,TValue>
#if LAMELLA_SURFACE_NETFX_2_0
namespace System.Collections.Generic
{
    /// <summary>A collection of key/value pairs in which each key appears at most once.</summary>
    /// <typeparam name="TKey">The type of the keys.</typeparam>
    /// <typeparam name="TValue">The type of the values.</typeparam>
    public interface IDictionary<TKey, TValue> : ICollection<KeyValuePair<TKey, TValue>>
    {
        /// <summary>The value stored under <paramref name="key"/>.</summary>
        /// <param name="key">The key to look up or store under.</param>
        TValue this[TKey key] { get; set; }

        /// <summary>The keys of the dictionary.</summary>
        ICollection<TKey> Keys { get; }

        /// <summary>The values of the dictionary.</summary>
        ICollection<TValue> Values { get; }

        /// <summary>Adds a new key and its value.</summary>
        /// <param name="key">The key, which must not already be present.</param>
        /// <param name="value">The value stored under <paramref name="key"/>.</param>
        void Add(TKey key, TValue value);

        /// <summary>Whether <paramref name="key"/> is present.</summary>
        /// <param name="key">The key to look for.</param>
        /// <returns>True when the key is present.</returns>
        bool ContainsKey(TKey key);

        /// <summary>Removes <paramref name="key"/> and its value.</summary>
        /// <param name="key">The key to remove.</param>
        /// <returns>True when the key was present and has been removed.</returns>
        bool Remove(TKey key);

        /// <summary>Reads the value stored under <paramref name="key"/>, if there is one.</summary>
        /// <param name="key">The key to look up.</param>
        /// <param name="value">The stored value, or the default of <typeparamref name="TValue"/> when the key is absent.</param>
        /// <returns>True when the key is present.</returns>
        bool TryGetValue(TKey key, out TValue value);
    }
}
#endif
