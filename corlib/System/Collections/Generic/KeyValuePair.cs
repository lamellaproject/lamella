// Lamella managed corlib (from scratch). -- System.Collections.Generic.KeyValuePair<TKey,TValue>
#if LAMELLA_SURFACE_NETFX_2_0
namespace System.Collections.Generic
{

    /// <summary>A key and the value stored under it, as one value.</summary>
    /// <typeparam name="TKey">The type of the key.</typeparam>
    /// <typeparam name="TValue">The type of the value.</typeparam>
    public struct KeyValuePair<TKey, TValue>
    {
        private TKey key;
        private TValue value;

        /// <summary>Pairs <paramref name="key"/> with <paramref name="value"/>.</summary>
        /// <param name="key">The key.</param>
        /// <param name="value">The value stored under the key.</param>
        public KeyValuePair(TKey key, TValue value)
        {
            this.key = key;
            this.value = value;
        }

        /// <summary>The key.</summary>
        public TKey Key
        {
            get { return key; }
        }

        /// <summary>The value.</summary>
        public TValue Value
        {
            get { return value; }
        }

        /// <summary>The pair as <c>[key, value]</c>, where a null key or value is written as nothing.</summary>
        /// <returns>The text of the pair.</returns>
        public override string ToString()
        {
            object k = key;
            object v = value;
            string keyText = k == null ? "" : k.ToString();
            string valueText = v == null ? "" : v.ToString();
            return "[" + keyText + ", " + valueText + "]";
        }
    }
}
#endif
