// Lamella managed corlib (from scratch). -- System.Collections.Generic.IReadOnlyDictionary<TKey,TValue>
#if LAMELLA_SURFACE_NETFX_4_5
namespace System.Collections.Generic
{
    /// <summary>A read-only view of key/value pairs in which each key appears at most once.</summary>
    /// <typeparam name="TKey">The type of the keys.</typeparam>
    /// <typeparam name="TValue">The type of the values.</typeparam>
    public interface IReadOnlyDictionary<TKey, TValue> : IReadOnlyCollection<KeyValuePair<TKey, TValue>>
    {
        /// <summary>The value stored under <paramref name="key"/>.</summary>
        /// <param name="key">The key to look up.</param>
        TValue this[TKey key] { get; }

        /// <summary>The keys, as a sequence.</summary>
        IEnumerable<TKey> Keys { get; }

        /// <summary>The values, as a sequence.</summary>
        IEnumerable<TValue> Values { get; }

        /// <summary>Whether <paramref name="key"/> is present.</summary>
        /// <param name="key">The key to look for.</param>
        /// <returns>True when the key is present.</returns>
        bool ContainsKey(TKey key);

        /// <summary>Reads the value stored under <paramref name="key"/>, if there is one.</summary>
        /// <param name="key">The key to look up.</param>
        /// <param name="value">The stored value, or the default of <typeparamref name="TValue"/> when the key is absent.</param>
        /// <returns>True when the key is present.</returns>
        bool TryGetValue(TKey key, out TValue value);
    }
}
#endif
