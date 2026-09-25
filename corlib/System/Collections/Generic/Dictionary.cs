// Lamella managed corlib (from scratch). -- System.Collections.Generic.Dictionary<TKey,TValue>
#if LAMELLA_SURFACE_NETFX_2_0
namespace System.Collections.Generic
{

    /// <summary>A collection of key/value pairs, looked up by key through an equality comparer.</summary>
    /// <typeparam name="TKey">The type of the keys. A key cannot be null.</typeparam>
    /// <typeparam name="TValue">The type of the values. A value can be null when the type allows it.</typeparam>
    /// <remarks>
    /// Pairs are held in slots and the slots are chained into hash buckets. Enumeration visits the
    /// slots in order, so pairs come back in the order they were added until one is removed; a
    /// later addition then takes the most recently vacated slot. Lookup, insertion and removal take
    /// constant time on average.
    /// </remarks>
    public class Dictionary<TKey, TValue> : IDictionary<TKey, TValue>, ICollection<KeyValuePair<TKey, TValue> >,
        IEnumerable<KeyValuePair<TKey, TValue> >, IDictionary, ICollection, IEnumerable
#if LAMELLA_SURFACE_NETFX_4_5
        , IReadOnlyDictionary<TKey, TValue>, IReadOnlyCollection<KeyValuePair<TKey, TValue>>
#endif
    {
        private const int FreeSlot = -1;

        private int[] buckets;
        private int[] hashes;
        private int[] next;
        private TKey[] keys;
        private TValue[] values;
        private int shift;
        private int used;
        private int freeHead;
        private int freeCount;
        private int version;
        private IEqualityComparer<TKey> comparer;
        private bool keyCanBeNull;
        private KeyCollection keyCollection;
        private ValueCollection valueCollection;


        /// <summary>An empty dictionary that compares keys with the default equality comparer for <typeparamref name="TKey"/>.</summary>
        public Dictionary()
        {
            Initialize(0, null);
        }

        /// <summary>An empty dictionary with room for <paramref name="capacity"/> pairs before it grows.</summary>
        /// <param name="capacity">How many pairs to make room for.</param>
        /// <exception cref="ArgumentOutOfRangeException"><paramref name="capacity"/> is negative.</exception>
        public Dictionary(int capacity)
        {
            Initialize(capacity, null);
        }

        /// <summary>An empty dictionary that compares keys with <paramref name="comparer"/>.</summary>
        /// <param name="comparer">The key comparer, or null for the default comparer for <typeparamref name="TKey"/>.</param>
        public Dictionary(IEqualityComparer<TKey> comparer)
        {
            Initialize(0, comparer);
        }

        /// <summary>An empty dictionary with room for <paramref name="capacity"/> pairs, comparing keys with <paramref name="comparer"/>.</summary>
        /// <param name="capacity">How many pairs to make room for.</param>
        /// <param name="comparer">The key comparer, or null for the default comparer for <typeparamref name="TKey"/>.</param>
        /// <exception cref="ArgumentOutOfRangeException"><paramref name="capacity"/> is negative.</exception>
        public Dictionary(int capacity, IEqualityComparer<TKey> comparer)
        {
            Initialize(capacity, comparer);
        }

        /// <summary>A dictionary holding a copy of every pair in <paramref name="dictionary"/>, compared with the default comparer.</summary>
        /// <param name="dictionary">The pairs to copy.</param>
        /// <exception cref="ArgumentNullException"><paramref name="dictionary"/> is null.</exception>
        /// <exception cref="ArgumentException"><paramref name="dictionary"/> holds two keys the default comparer considers equal.</exception>
        public Dictionary(IDictionary<TKey, TValue> dictionary)
        {
            CopyFrom(dictionary, null);
        }

        /// <summary>A dictionary holding a copy of every pair in <paramref name="dictionary"/>, compared with <paramref name="comparer"/>.</summary>
        /// <param name="dictionary">The pairs to copy, added in the order <paramref name="dictionary"/> enumerates them.</param>
        /// <param name="comparer">The key comparer, or null for the default comparer for <typeparamref name="TKey"/>.</param>
        /// <exception cref="ArgumentNullException"><paramref name="dictionary"/> is null.</exception>
        /// <exception cref="ArgumentException"><paramref name="dictionary"/> holds two keys <paramref name="comparer"/> considers equal.</exception>
        public Dictionary(IDictionary<TKey, TValue> dictionary, IEqualityComparer<TKey> comparer)
        {
            CopyFrom(dictionary, comparer);
        }

        private void Initialize(int capacity, IEqualityComparer<TKey> comparer)
        {
            if (capacity < 0) throw new ArgumentOutOfRangeException("capacity");
            if (capacity > 0) Allocate(capacity);
            if (comparer == null) comparer = EqualityComparer<TKey>.Default;
            this.comparer = comparer;
            object defaultKey = default(TKey);
            keyCanBeNull = defaultKey == null;
        }

        private void CopyFrom(IDictionary<TKey, TValue> dictionary, IEqualityComparer<TKey> comparer)
        {
            if (dictionary == null) throw new ArgumentNullException("dictionary");
            Initialize(dictionary.Count, comparer);
            foreach (KeyValuePair<TKey, TValue> pair in dictionary)
            {
                Add(pair.Key, pair.Value);
            }
        }

        /// <summary>The comparer that decides whether two keys are equal.</summary>
        public IEqualityComparer<TKey> Comparer
        {
            get { return comparer; }
        }

        /// <summary>How many pairs the dictionary holds.</summary>
        public int Count
        {
            get { return used - freeCount; }
        }

        /// <summary>The value stored under <paramref name="key"/>. Setting it adds the key, or replaces the value of a key already present.</summary>
        /// <param name="key">The key to read or write.</param>
        /// <exception cref="ArgumentNullException"><paramref name="key"/> is null.</exception>
        /// <exception cref="KeyNotFoundException">The key is read and is not present.</exception>
        public TValue this[TKey key]
        {
            get
            {
                int slot = FindSlot(key);
                if (slot < 0)
                {
                    throw new KeyNotFoundException("The given key '" + KeyText(key) + "' was not present in the dictionary.");
                }
                return values[slot];
            }
            set
            {
                Insert(key, value, ReplaceExisting);
            }
        }

        /// <summary>The keys, as a live view of the dictionary in enumeration order.</summary>
        public KeyCollection Keys
        {
            get
            {
                if (keyCollection == null) keyCollection = new KeyCollection(this);
                return keyCollection;
            }
        }

        /// <summary>The values, as a live view of the dictionary in enumeration order.</summary>
        public ValueCollection Values
        {
            get
            {
                if (valueCollection == null) valueCollection = new ValueCollection(this);
                return valueCollection;
            }
        }

        /// <summary>Adds <paramref name="key"/> with <paramref name="value"/>.</summary>
        /// <param name="key">The key, which must not already be present.</param>
        /// <param name="value">The value to store under it.</param>
        /// <exception cref="ArgumentNullException"><paramref name="key"/> is null.</exception>
        /// <exception cref="ArgumentException">The key is already present.</exception>
        public void Add(TKey key, TValue value)
        {
            Insert(key, value, ThrowOnExisting);
        }

#if LAMELLA_SURFACE_NETCORE_2_0
        /// <summary>Adds <paramref name="key"/> with <paramref name="value"/> unless the key is already present.</summary>
        /// <param name="key">The key to add.</param>
        /// <param name="value">The value to store under it.</param>
        /// <returns>True when the pair was added; false when the key was already present, whose value is left as it was.</returns>
        /// <exception cref="ArgumentNullException"><paramref name="key"/> is null.</exception>
        public bool TryAdd(TKey key, TValue value)
        {
            return Insert(key, value, KeepExisting);
        }
#endif

        /// <summary>Removes every pair. Any room the dictionary has already made is kept.</summary>
        public void Clear()
        {
            if (used == 0) return;
            for (int bucket = 0; bucket < buckets.Length; bucket++)
            {
                buckets[bucket] = 0;
            }
            for (int slot = 0; slot < used; slot++)
            {
                hashes[slot] = FreeSlot;
                next[slot] = -1;
                keys[slot] = default(TKey);
                values[slot] = default(TValue);
            }
            used = 0;
            freeCount = 0;
        }

        /// <summary>Whether <paramref name="key"/> is present.</summary>
        /// <param name="key">The key to look for.</param>
        /// <returns>True when the key is present.</returns>
        /// <exception cref="ArgumentNullException"><paramref name="key"/> is null.</exception>
        public bool ContainsKey(TKey key)
        {
            return FindSlot(key) >= 0;
        }

        /// <summary>Whether any pair holds a value equal to <paramref name="value"/>, by the default comparer for <typeparamref name="TValue"/>.</summary>
        /// <param name="value">The value to look for; null matches a null value.</param>
        /// <returns>True when a matching value is present.</returns>
        /// <remarks>This walks every pair.</remarks>
        public bool ContainsValue(TValue value)
        {
            EqualityComparer<TValue> valueComparer = EqualityComparer<TValue>.Default;
            for (int slot = 0; slot < used; slot++)
            {
                if (hashes[slot] >= 0 && valueComparer.Equals(values[slot], value)) return true;
            }
            return false;
        }

        /// <summary>An enumerator over the pairs, in slot order.</summary>
        /// <returns>The enumerator.</returns>
        public Enumerator GetEnumerator()
        {
            return new Enumerator(this, false);
        }

        /// <summary>Removes <paramref name="key"/> and its value.</summary>
        /// <param name="key">The key to remove.</param>
        /// <returns>True when the key was present and has been removed; false when it was not present.</returns>
        /// <exception cref="ArgumentNullException"><paramref name="key"/> is null.</exception>
        public bool Remove(TKey key)
        {
            RequireKey(key);
            if (buckets == null) return false;
            int hash = HashOf(key);
            int bucket = BucketOf(hash);
            int previous = -1;
            int slot = buckets[bucket] - 1;
            while (slot >= 0)
            {
                if (hashes[slot] == hash && comparer.Equals(keys[slot], key))
                {
                    if (previous < 0)
                    {
                        buckets[bucket] = next[slot] + 1;
                    }
                    else
                    {
                        next[previous] = next[slot];
                    }
                    hashes[slot] = FreeSlot;
                    next[slot] = freeCount > 0 ? freeHead : -1;
                    keys[slot] = default(TKey);
                    values[slot] = default(TValue);
                    freeHead = slot;
                    freeCount = freeCount + 1;
                    return true;
                }
                previous = slot;
                slot = next[slot];
            }
            return false;
        }

        /// <summary>Reads the value stored under <paramref name="key"/>, if the key is present.</summary>
        /// <param name="key">The key to look up.</param>
        /// <param name="value">The stored value, or the default of <typeparamref name="TValue"/> when the key is absent.</param>
        /// <returns>True when the key is present.</returns>
        /// <exception cref="ArgumentNullException"><paramref name="key"/> is null.</exception>
        public bool TryGetValue(TKey key, out TValue value)
        {
            int slot = FindSlot(key);
            if (slot >= 0)
            {
                value = values[slot];
                return true;
            }
            value = default(TValue);
            return false;
        }

        // ---- ICollection<KeyValuePair<TKey, TValue>> -------------------------------------------

        bool ICollection<KeyValuePair<TKey, TValue>>.IsReadOnly
        {
            get { return false; }
        }

        void ICollection<KeyValuePair<TKey, TValue>>.Add(KeyValuePair<TKey, TValue> keyValuePair)
        {
            Add(keyValuePair.Key, keyValuePair.Value);
        }

        bool ICollection<KeyValuePair<TKey, TValue>>.Contains(KeyValuePair<TKey, TValue> keyValuePair)
        {
            int slot = FindSlot(keyValuePair.Key);
            return slot >= 0 && EqualityComparer<TValue>.Default.Equals(values[slot], keyValuePair.Value);
        }

        void ICollection<KeyValuePair<TKey, TValue>>.CopyTo(KeyValuePair<TKey, TValue>[] array, int index)
        {
            CopyPairsTo(array, index);
        }

        bool ICollection<KeyValuePair<TKey, TValue>>.Remove(KeyValuePair<TKey, TValue> keyValuePair)
        {
            int slot = FindSlot(keyValuePair.Key);
            if (slot >= 0 && EqualityComparer<TValue>.Default.Equals(values[slot], keyValuePair.Value))
            {
                Remove(keyValuePair.Key);
                return true;
            }
            return false;
        }

        // ---- IDictionary<TKey, TValue> and IReadOnlyDictionary<TKey, TValue> -------------------

        ICollection<TKey> IDictionary<TKey, TValue>.Keys
        {
            get { return Keys; }
        }

        ICollection<TValue> IDictionary<TKey, TValue>.Values
        {
            get { return Values; }
        }

#if LAMELLA_SURFACE_NETFX_4_5
        IEnumerable<TKey> IReadOnlyDictionary<TKey, TValue>.Keys
        {
            get { return Keys; }
        }

        IEnumerable<TValue> IReadOnlyDictionary<TKey, TValue>.Values
        {
            get { return Values; }
        }
#endif

        // ---- the enumerable interfaces -------------------------------------------------------------

        IEnumerator<KeyValuePair<TKey, TValue>> IEnumerable<KeyValuePair<TKey, TValue>>.GetEnumerator()
        {
            return new Enumerator(this, false);
        }

        IEnumerator IEnumerable.GetEnumerator()
        {
            return new Enumerator(this, false);
        }

        // ---- ICollection and IDictionary (non-generic) ---------------------------------------------

        bool ICollection.IsSynchronized
        {
            get { return false; }
        }

        object ICollection.SyncRoot
        {
            get { return this; }
        }

        void ICollection.CopyTo(Array array, int index)
        {
            CheckCopyTarget(array, index);
            KeyValuePair<TKey, TValue>[] pairs = array as KeyValuePair<TKey, TValue>[];
            if (pairs != null)
            {
                CopyPairsTo(pairs, index);
                return;
            }
            DictionaryEntry[] entries = array as DictionaryEntry[];
            if (entries != null)
            {
                for (int slot = 0; slot < used; slot++)
                {
                    if (hashes[slot] < 0) continue;
                    entries[index] = new DictionaryEntry(keys[slot], values[slot]);
                    index = index + 1;
                }
                return;
            }
            object[] objects = array as object[];
            if (objects == null) throw IncompatibleTarget();
            try
            {
                for (int slot = 0; slot < used; slot++)
                {
                    if (hashes[slot] < 0) continue;
                    objects[index] = new KeyValuePair<TKey, TValue>(keys[slot], values[slot]);
                    index = index + 1;
                }
            }
            catch (ArrayTypeMismatchException)
            {
                throw IncompatibleTarget();
            }
        }

        bool IDictionary.IsFixedSize
        {
            get { return false; }
        }

        bool IDictionary.IsReadOnly
        {
            get { return false; }
        }

        ICollection IDictionary.Keys
        {
            get { return Keys; }
        }

        ICollection IDictionary.Values
        {
            get { return Values; }
        }

        object IDictionary.this[object key]
        {
            get
            {
                if (key == null) throw new ArgumentNullException("key");
                if (key is TKey)
                {
                    int slot = FindSlot((TKey)key);
                    if (slot >= 0) return values[slot];
                }
                return null;
            }
            set
            {
                CheckUntypedPair(key, value);
                this[(TKey)key] = (TValue)value;
            }
        }

        void IDictionary.Add(object key, object value)
        {
            CheckUntypedPair(key, value);
            Add((TKey)key, (TValue)value);
        }

        bool IDictionary.Contains(object key)
        {
            if (key == null) throw new ArgumentNullException("key");
            return key is TKey && ContainsKey((TKey)key);
        }

        IDictionaryEnumerator IDictionary.GetEnumerator()
        {
            return new Enumerator(this, true);
        }

        void IDictionary.Remove(object key)
        {
            if (key == null) throw new ArgumentNullException("key");
            if (key is TKey) Remove((TKey)key);
        }

        // ---- the table -------------------------------------------------------------------------

        private void Allocate(int capacity)
        {
            if (capacity > 0x40000000) throw new OutOfMemoryException();
            int size = 4;
            int bits = 2;
            while (size < capacity)
            {
                size = size * 2;
                bits = bits + 1;
            }
            buckets = new int[size];
            hashes = new int[size];
            next = new int[size];
            keys = new TKey[size];
            values = new TValue[size];
            shift = 32 - bits;
        }

        private void Grow()
        {
            int size = hashes.Length * 2;
            if (size <= 0) throw new OutOfMemoryException();
            int[] biggerHashes = new int[size];
            int[] biggerNext = new int[size];
            TKey[] biggerKeys = new TKey[size];
            TValue[] biggerValues = new TValue[size];
            for (int slot = 0; slot < used; slot++)
            {
                biggerHashes[slot] = hashes[slot];
                biggerKeys[slot] = keys[slot];
                biggerValues[slot] = values[slot];
            }
            hashes = biggerHashes;
            next = biggerNext;
            keys = biggerKeys;
            values = biggerValues;
            buckets = new int[size];
            shift = shift - 1;
            for (int slot = 0; slot < used; slot++)
            {
                int bucket = BucketOf(hashes[slot]);
                next[slot] = buckets[bucket] - 1;
                buckets[bucket] = slot + 1;
            }
        }

        private void RequireKey(TKey key)
        {
            if (keyCanBeNull && (object)key == null) throw new ArgumentNullException("key");
        }

        private int HashOf(TKey key)
        {
            return comparer.GetHashCode(key) & 0x7FFFFFFF;
        }

        private int BucketOf(int hash)
        {
            return (int)(unchecked((uint)hash * 2654435769u) >> shift);
        }

        private int FindSlot(TKey key)
        {
            RequireKey(key);
            if (buckets == null) return -1;
            int hash = HashOf(key);
            int slot = buckets[BucketOf(hash)] - 1;
            while (slot >= 0)
            {
                if (hashes[slot] == hash && comparer.Equals(keys[slot], key)) return slot;
                slot = next[slot];
            }
            return -1;
        }

        private const int ReplaceExisting = 0;
        private const int ThrowOnExisting = 1;
        private const int KeepExisting = 2;

        private bool Insert(TKey key, TValue value, int onExisting)
        {
            RequireKey(key);
            if (buckets == null) Allocate(0);
            int hash = HashOf(key);
            int bucket = BucketOf(hash);
            int slot = buckets[bucket] - 1;
            while (slot >= 0)
            {
                if (hashes[slot] == hash && comparer.Equals(keys[slot], key))
                {
                    if (onExisting == ThrowOnExisting)
                    {
                        throw new ArgumentException("An item with the same key has already been added. Key: " + KeyText(key));
                    }
                    if (onExisting == KeepExisting) return false;
                    values[slot] = value;
                    return true;
                }
                slot = next[slot];
            }
            if (freeCount > 0)
            {
                slot = freeHead;
                freeHead = next[slot];
                freeCount = freeCount - 1;
            }
            else
            {
                if (used == hashes.Length)
                {
                    Grow();
                    bucket = BucketOf(hash);
                }
                slot = used;
                used = used + 1;
            }
            hashes[slot] = hash;
            keys[slot] = key;
            values[slot] = value;
            next[slot] = buckets[bucket] - 1;
            buckets[bucket] = slot + 1;
            version = version + 1;
            return true;
        }

        private static string KeyText(TKey key)
        {
            object boxed = key;
            return boxed == null ? "" : boxed.ToString();
        }

        private void CheckUntypedPair(object key, object value)
        {
            if (key == null) throw new ArgumentNullException("key");
            if (value == null)
            {
                object defaultValue = default(TValue);
                if (defaultValue != null) throw new ArgumentNullException("value");
            }
            if (!(key is TKey))
            {
                throw new ArgumentException("The value \"" + key + "\" is not of the key type and cannot be used in this generic collection.", "key");
            }
            if (value != null && !(value is TValue))
            {
                throw new ArgumentException("The value \"" + value + "\" is not of the value type and cannot be used in this generic collection.", "value");
            }
        }

        // ---- copying out -----------------------------------------------------------------------

        private void CheckCopyTarget(Array array, int index)
        {
            if (array == null) throw new ArgumentNullException("array");
            if (array.Rank != 1) throw new ArgumentException("Only single dimensional arrays are supported for the requested action.");
            if (array.GetLowerBound(0) != 0) throw new ArgumentException("The lower bound of target array must be zero.");
            CheckCopyRange(array.Length, index);
        }

        private void CheckCopyRange(int length, int index)
        {
            if (index < 0 || index > length) throw new ArgumentOutOfRangeException("index");
            if (length - index < Count)
            {
                throw new ArgumentException("Destination array is not long enough to copy all the items in the collection. Check array index and length.");
            }
        }

        private static ArgumentException IncompatibleTarget()
        {
            return new ArgumentException("Target array type is not compatible with the type of items in the collection.");
        }

        private void CopyPairsTo(KeyValuePair<TKey, TValue>[] array, int index)
        {
            if (array == null) throw new ArgumentNullException("array");
            CheckCopyRange(array.Length, index);
            for (int slot = 0; slot < used; slot++)
            {
                if (hashes[slot] < 0) continue;
                array[index] = new KeyValuePair<TKey, TValue>(keys[slot], values[slot]);
                index = index + 1;
            }
        }

        private void CopyKeysTo(TKey[] array, int index)
        {
            if (array == null) throw new ArgumentNullException("array");
            CheckCopyRange(array.Length, index);
            for (int slot = 0; slot < used; slot++)
            {
                if (hashes[slot] < 0) continue;
                array[index] = keys[slot];
                index = index + 1;
            }
        }

        private void CopyValuesTo(TValue[] array, int index)
        {
            if (array == null) throw new ArgumentNullException("array");
            CheckCopyRange(array.Length, index);
            for (int slot = 0; slot < used; slot++)
            {
                if (hashes[slot] < 0) continue;
                array[index] = values[slot];
                index = index + 1;
            }
        }

        private void CopyKeysTo(Array array, int index)
        {
            CheckCopyTarget(array, index);
            TKey[] typed = array as TKey[];
            if (typed != null)
            {
                CopyKeysTo(typed, index);
                return;
            }
            object[] objects = array as object[];
            if (objects == null) throw IncompatibleTarget();
            try
            {
                for (int slot = 0; slot < used; slot++)
                {
                    if (hashes[slot] < 0) continue;
                    objects[index] = keys[slot];
                    index = index + 1;
                }
            }
            catch (ArrayTypeMismatchException)
            {
                throw IncompatibleTarget();
            }
        }

        private void CopyValuesTo(Array array, int index)
        {
            CheckCopyTarget(array, index);
            TValue[] typed = array as TValue[];
            if (typed != null)
            {
                CopyValuesTo(typed, index);
                return;
            }
            object[] objects = array as object[];
            if (objects == null) throw IncompatibleTarget();
            try
            {
                for (int slot = 0; slot < used; slot++)
                {
                    if (hashes[slot] < 0) continue;
                    objects[index] = values[slot];
                    index = index + 1;
                }
            }
            catch (ArrayTypeMismatchException)
            {
                throw IncompatibleTarget();
            }
        }

        // ---- what the nested types call back into ----------------------------------------------

        private int NextOccupied(int expectedVersion, int from)
        {
            CheckVersion(expectedVersion);
            while (from < used)
            {
                if (hashes[from] >= 0) return from;
                from = from + 1;
            }
            return -1;
        }

        private void CheckVersion(int expectedVersion)
        {
            if (expectedVersion != version)
            {
                throw new InvalidOperationException("Collection was modified; enumeration operation may not execute.");
            }
        }

        private void CheckPositioned(bool positioned)
        {
            if (!positioned)
            {
                throw new InvalidOperationException("Enumeration has either not started or has already finished.");
            }
        }

        private KeyValuePair<TKey, TValue> PairAt(int slot)
        {
            return new KeyValuePair<TKey, TValue>(keys[slot], values[slot]);
        }

        private KeyValuePair<TKey, TValue> EmptyPair()
        {
            return default(KeyValuePair<TKey, TValue>);
        }

        private TKey EmptyKey()
        {
            return default(TKey);
        }

        private TValue EmptyValue()
        {
            return default(TValue);
        }

        /// <summary>Enumerates the pairs of a <see cref="Dictionary{TKey, TValue}"/>.</summary>
        /// <remarks>
        /// Adding a key to the dictionary invalidates the enumerator: its next MoveNext or Reset
        /// throws InvalidOperationException. Removing a key, clearing the dictionary, or replacing the
        /// value of a key already present does not.
        /// </remarks>
        public struct Enumerator : IEnumerator<KeyValuePair<TKey, TValue> >, IDictionaryEnumerator, IEnumerator, IDisposable
        {
            private Dictionary<TKey, TValue> dictionary;
            private int version;
            private int index;
            private bool positioned;
            private bool entries;
            private KeyValuePair<TKey, TValue> current;

            internal Enumerator(Dictionary<TKey, TValue> dictionary, bool entries)
            {
                this.dictionary = dictionary;
                this.version = dictionary.version;
                this.index = 0;
                this.positioned = false;
                this.entries = entries;
                this.current = dictionary.EmptyPair();
            }

            /// <summary>Advances to the next pair.</summary>
            /// <returns>True when positioned on a pair; false past the last one.</returns>
            /// <exception cref="InvalidOperationException">A key was added to the dictionary after this enumerator was created.</exception>
            public bool MoveNext()
            {
                int slot = dictionary.NextOccupied(version, index);
                if (slot < 0)
                {
                    index = dictionary.used;
                    positioned = false;
                    current = dictionary.EmptyPair();
                    return false;
                }
                index = slot + 1;
                positioned = true;
                current = dictionary.PairAt(slot);
                return true;
            }

            /// <summary>The pair at the cursor; the default pair before the first MoveNext and after the last.</summary>
            public KeyValuePair<TKey, TValue> Current
            {
                get { return current; }
            }

            object IEnumerator.Current
            {
                get
                {
                    dictionary.CheckPositioned(positioned);
                    if (entries) return new DictionaryEntry(current.Key, current.Value);
                    return current;
                }
            }

            DictionaryEntry IDictionaryEnumerator.Entry
            {
                get
                {
                    dictionary.CheckPositioned(positioned);
                    return new DictionaryEntry(current.Key, current.Value);
                }
            }

            object IDictionaryEnumerator.Key
            {
                get
                {
                    dictionary.CheckPositioned(positioned);
                    return current.Key;
                }
            }

            object IDictionaryEnumerator.Value
            {
                get
                {
                    dictionary.CheckPositioned(positioned);
                    return current.Value;
                }
            }

            void IEnumerator.Reset()
            {
                dictionary.CheckVersion(version);
                index = 0;
                positioned = false;
                current = dictionary.EmptyPair();
            }

            /// <summary>Releases nothing; present for the enumerator pattern.</summary>
            public void Dispose()
            {
            }
        }

        /// <summary>The keys of a <see cref="Dictionary{TKey, TValue}"/>, as a read-only live view.</summary>
        /// <remarks>The keys come back in the dictionary's enumeration order. Every mutating member throws NotSupportedException.</remarks>
        public sealed class KeyCollection : ICollection<TKey>, IEnumerable<TKey>, ICollection, IEnumerable
        {
            private Dictionary<TKey, TValue> dictionary;

            /// <summary>A view of the keys of <paramref name="dictionary"/>.</summary>
            /// <param name="dictionary">The dictionary to view.</param>
            /// <exception cref="ArgumentNullException"><paramref name="dictionary"/> is null.</exception>
            public KeyCollection(Dictionary<TKey, TValue> dictionary)
            {
                if (dictionary == null) throw new ArgumentNullException("dictionary");
                this.dictionary = dictionary;
            }

            /// <summary>How many keys the dictionary holds.</summary>
            public int Count
            {
                get { return dictionary.Count; }
            }

            /// <summary>Copies the keys into <paramref name="array"/>, starting at <paramref name="index"/>.</summary>
            /// <param name="array">The destination array.</param>
            /// <param name="index">The first index of <paramref name="array"/> written.</param>
            /// <exception cref="ArgumentNullException"><paramref name="array"/> is null.</exception>
            /// <exception cref="ArgumentOutOfRangeException"><paramref name="index"/> is negative or past the end of <paramref name="array"/>.</exception>
            /// <exception cref="ArgumentException">The keys do not fit between <paramref name="index"/> and the end of <paramref name="array"/>.</exception>
            public void CopyTo(TKey[] array, int index)
            {
                dictionary.CopyKeysTo(array, index);
            }

            /// <summary>An enumerator over the keys.</summary>
            /// <returns>The enumerator.</returns>
            public Enumerator GetEnumerator()
            {
                return new Enumerator(dictionary);
            }

            bool ICollection<TKey>.IsReadOnly
            {
                get { return true; }
            }

            void ICollection<TKey>.Add(TKey item)
            {
                throw new NotSupportedException("Mutating a key collection derived from a dictionary is not allowed.");
            }

            void ICollection<TKey>.Clear()
            {
                throw new NotSupportedException("Mutating a key collection derived from a dictionary is not allowed.");
            }

            bool ICollection<TKey>.Contains(TKey item)
            {
                return dictionary.ContainsKey(item);
            }

            bool ICollection<TKey>.Remove(TKey item)
            {
                throw new NotSupportedException("Mutating a key collection derived from a dictionary is not allowed.");
            }

            IEnumerator<TKey> IEnumerable<TKey>.GetEnumerator()
            {
                return new Enumerator(dictionary);
            }

            IEnumerator IEnumerable.GetEnumerator()
            {
                return new Enumerator(dictionary);
            }

            void ICollection.CopyTo(Array array, int index)
            {
                dictionary.CopyKeysTo(array, index);
            }

            bool ICollection.IsSynchronized
            {
                get { return false; }
            }

            object ICollection.SyncRoot
            {
                get { return ((ICollection)dictionary).SyncRoot; }
            }

            /// <summary>Enumerates the keys of a <see cref="Dictionary{TKey, TValue}"/>.</summary>
            /// <remarks>It is invalidated by the same changes that invalidate the dictionary's own enumerator.</remarks>
            public struct Enumerator : IEnumerator<TKey>, IEnumerator, IDisposable
            {
                private Dictionary<TKey, TValue> dictionary;
                private int version;
                private int index;
                private bool positioned;
                private TKey current;

                internal Enumerator(Dictionary<TKey, TValue> dictionary)
                {
                    this.dictionary = dictionary;
                    this.version = dictionary.version;
                    this.index = 0;
                    this.positioned = false;
                    this.current = dictionary.EmptyKey();
                }

                /// <summary>Advances to the next key.</summary>
                /// <returns>True when positioned on a key; false past the last one.</returns>
                /// <exception cref="InvalidOperationException">A key was added to the dictionary after this enumerator was created.</exception>
                public bool MoveNext()
                {
                    int slot = dictionary.NextOccupied(version, index);
                    if (slot < 0)
                    {
                        index = dictionary.used;
                        positioned = false;
                        current = dictionary.EmptyKey();
                        return false;
                    }
                    index = slot + 1;
                    positioned = true;
                    current = dictionary.keys[slot];
                    return true;
                }

                /// <summary>The key at the cursor; the default key before the first MoveNext and after the last.</summary>
                public TKey Current
                {
                    get { return current; }
                }

                object IEnumerator.Current
                {
                    get
                    {
                        dictionary.CheckPositioned(positioned);
                        return current;
                    }
                }

                void IEnumerator.Reset()
                {
                    dictionary.CheckVersion(version);
                    index = 0;
                    positioned = false;
                    current = dictionary.EmptyKey();
                }

                /// <summary>Releases nothing; present for the enumerator pattern.</summary>
                public void Dispose()
                {
                }
            }
        }

        /// <summary>The values of a <see cref="Dictionary{TKey, TValue}"/>, as a read-only live view.</summary>
        /// <remarks>The values come back in the dictionary's enumeration order. Every mutating member throws NotSupportedException.</remarks>
        public sealed class ValueCollection : ICollection<TValue>, IEnumerable<TValue>, ICollection, IEnumerable
        {
            private Dictionary<TKey, TValue> dictionary;

            /// <summary>A view of the values of <paramref name="dictionary"/>.</summary>
            /// <param name="dictionary">The dictionary to view.</param>
            /// <exception cref="ArgumentNullException"><paramref name="dictionary"/> is null.</exception>
            public ValueCollection(Dictionary<TKey, TValue> dictionary)
            {
                if (dictionary == null) throw new ArgumentNullException("dictionary");
                this.dictionary = dictionary;
            }

            /// <summary>How many values the dictionary holds.</summary>
            public int Count
            {
                get { return dictionary.Count; }
            }

            /// <summary>Copies the values into <paramref name="array"/>, starting at <paramref name="index"/>.</summary>
            /// <param name="array">The destination array.</param>
            /// <param name="index">The first index of <paramref name="array"/> written.</param>
            /// <exception cref="ArgumentNullException"><paramref name="array"/> is null.</exception>
            /// <exception cref="ArgumentOutOfRangeException"><paramref name="index"/> is negative or past the end of <paramref name="array"/>.</exception>
            /// <exception cref="ArgumentException">The values do not fit between <paramref name="index"/> and the end of <paramref name="array"/>.</exception>
            public void CopyTo(TValue[] array, int index)
            {
                dictionary.CopyValuesTo(array, index);
            }

            /// <summary>An enumerator over the values.</summary>
            /// <returns>The enumerator.</returns>
            public Enumerator GetEnumerator()
            {
                return new Enumerator(dictionary);
            }

            bool ICollection<TValue>.IsReadOnly
            {
                get { return true; }
            }

            void ICollection<TValue>.Add(TValue item)
            {
                throw new NotSupportedException("Mutating a value collection derived from a dictionary is not allowed.");
            }

            void ICollection<TValue>.Clear()
            {
                throw new NotSupportedException("Mutating a value collection derived from a dictionary is not allowed.");
            }

            bool ICollection<TValue>.Contains(TValue item)
            {
                return dictionary.ContainsValue(item);
            }

            bool ICollection<TValue>.Remove(TValue item)
            {
                throw new NotSupportedException("Mutating a value collection derived from a dictionary is not allowed.");
            }

            IEnumerator<TValue> IEnumerable<TValue>.GetEnumerator()
            {
                return new Enumerator(dictionary);
            }

            IEnumerator IEnumerable.GetEnumerator()
            {
                return new Enumerator(dictionary);
            }

            void ICollection.CopyTo(Array array, int index)
            {
                dictionary.CopyValuesTo(array, index);
            }

            bool ICollection.IsSynchronized
            {
                get { return false; }
            }

            object ICollection.SyncRoot
            {
                get { return ((ICollection)dictionary).SyncRoot; }
            }

            /// <summary>Enumerates the values of a <see cref="Dictionary{TKey, TValue}"/>.</summary>
            /// <remarks>It is invalidated by the same changes that invalidate the dictionary's own enumerator.</remarks>
            public struct Enumerator : IEnumerator<TValue>, IEnumerator, IDisposable
            {
                private Dictionary<TKey, TValue> dictionary;
                private int version;
                private int index;
                private bool positioned;
                private TValue current;

                internal Enumerator(Dictionary<TKey, TValue> dictionary)
                {
                    this.dictionary = dictionary;
                    this.version = dictionary.version;
                    this.index = 0;
                    this.positioned = false;
                    this.current = dictionary.EmptyValue();
                }

                /// <summary>Advances to the next value.</summary>
                /// <returns>True when positioned on a value; false past the last one.</returns>
                /// <exception cref="InvalidOperationException">A key was added to the dictionary after this enumerator was created.</exception>
                public bool MoveNext()
                {
                    int slot = dictionary.NextOccupied(version, index);
                    if (slot < 0)
                    {
                        index = dictionary.used;
                        positioned = false;
                        current = dictionary.EmptyValue();
                        return false;
                    }
                    index = slot + 1;
                    positioned = true;
                    current = dictionary.values[slot];
                    return true;
                }

                /// <summary>The value at the cursor; the default value before the first MoveNext and after the last.</summary>
                public TValue Current
                {
                    get { return current; }
                }

                object IEnumerator.Current
                {
                    get
                    {
                        dictionary.CheckPositioned(positioned);
                        return current;
                    }
                }

                void IEnumerator.Reset()
                {
                    dictionary.CheckVersion(version);
                    index = 0;
                    positioned = false;
                    current = dictionary.EmptyValue();
                }

                /// <summary>Releases nothing; present for the enumerator pattern.</summary>
                public void Dispose()
                {
                }
            }
        }
    }
}
#endif
