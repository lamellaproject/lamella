// Lamella managed corlib (from scratch). -- System.Collections.Hashtable
namespace System.Collections
{
    public class Hashtable : IDictionary, ICloneable
    {
        private object[] keys;
        private object[] values;
        private int[] hashes;
        private const int Empty = -1;
        private const int Tombstone = -2;
        private int count;
        private int used;

        internal int version;

        private HashtableKeysOrValues keysView;
        private HashtableKeysOrValues valuesView;

        public Hashtable()
        {
            Initialize(8);
        }

        /// <summary>A table sized for about <paramref name="capacity"/> entries.</summary>
        /// <exception cref="ArgumentOutOfRangeException"><paramref name="capacity"/> is negative.</exception>
        public Hashtable(int capacity)
        {
            if (capacity < 0) throw new ArgumentOutOfRangeException("capacity");
            if (capacity > 0x40000000) throw new ArgumentException("capacity");
            int slots = 8;
            while (slots < capacity) slots = slots * 2;
            Initialize(slots);
        }

        private void Initialize(int capacity)
        {
            keys = new object[capacity];
            values = new object[capacity];
            hashes = new int[capacity];
            for (int i = 0; i < capacity; i++) hashes[i] = Empty;
            count = 0;
            used = 0;
        }

        public int Count { get { return count; } }

        public bool IsFixedSize { get { return false; } }
        public bool IsReadOnly { get { return false; } }

        public bool IsSynchronized { get { return false; } }
        public object SyncRoot { get { return this; } }

        private static void CheckKey(object key)
        {
            if (key == null) throw new ArgumentNullException("key");
        }

        private static int HashOf(object key)
        {
            return key.GetHashCode() & 0x7FFFFFFF;
        }

        private int FindSlot(object key)
        {
            int hash = HashOf(key);
            int capacity = hashes.Length;
            int index = hash % capacity;
            for (int probe = 0; probe < capacity; probe++)
            {
                int state = hashes[index];
                if (state == Empty) return -1;
                if (state == hash && keys[index] != null && keys[index].Equals(key)) return index;
                index = index + 1;
                if (index == capacity) index = 0;
            }
            return -1;
        }

        private void Insert(object key, object value, bool add)
        {
            if ((used + 1) * 4 >= hashes.Length * 3) Grow();
            int hash = HashOf(key);
            int capacity = hashes.Length;
            int index = hash % capacity;
            int tombstone = -1;
            for (int probe = 0; probe < capacity; probe++)
            {
                int state = hashes[index];
                if (state == Empty)
                {
                    if (tombstone >= 0)
                    {
                        index = tombstone;
                    }
                    else
                    {
                        used = used + 1;
                    }
                    hashes[index] = hash;
                    keys[index] = key;
                    values[index] = value;
                    count = count + 1;
                    version = version + 1;
                    return;
                }
                if (state == Tombstone)
                {
                    if (tombstone < 0) tombstone = index;
                }
                else if (state == hash && keys[index] != null && keys[index].Equals(key))
                {
                    if (add)
                    {
                        throw new ArgumentException("Item has already been added. Key in dictionary: '"
                            + keys[index].ToString() + "'  Key being added: '" + key.ToString() + "'");
                    }
                    values[index] = value;
                    version = version + 1;
                    return;
                }
                index = index + 1;
                if (index == capacity) index = 0;
            }
        }

        private void Grow()
        {
            object[] oldKeys = keys;
            object[] oldValues = values;
            int[] oldHashes = hashes;
            Initialize(oldHashes.Length * 2);
            for (int i = 0; i < oldHashes.Length; i++)
            {
                if (oldHashes[i] >= 0) InsertClean(oldKeys[i], oldValues[i], oldHashes[i]);
            }
        }

        private void InsertClean(object key, object value, int hash)
        {
            int capacity = hashes.Length;
            int index = hash % capacity;
            while (hashes[index] != Empty)
            {
                index = index + 1;
                if (index == capacity) index = 0;
            }
            hashes[index] = hash;
            keys[index] = key;
            values[index] = value;
            count = count + 1;
            used = used + 1;
        }

        public bool Contains(object key) { CheckKey(key); return FindSlot(key) >= 0; }
        public bool ContainsKey(object key) { CheckKey(key); return FindSlot(key) >= 0; }

        public bool ContainsValue(object value)
        {
            for (int i = 0; i < hashes.Length; i++)
            {
                if (hashes[i] < 0) continue;
                object v = values[i];
                if (value == null) { if (v == null) return true; }
                else if (v != null && v.Equals(value)) return true;
            }
            return false;
        }

        public object this[object key]
        {
            get
            {
                CheckKey(key);
                int i = FindSlot(key);
                return (i < 0) ? null : values[i];
            }
            set
            {
                CheckKey(key);
                Insert(key, value, false);
            }
        }

        public void Add(object key, object value)
        {
            CheckKey(key);
            Insert(key, value, true);
        }

        public object Clone()
        {
            Hashtable copy = new Hashtable(count);
            for (int i = 0; i < hashes.Length; i++)
            {
                if (hashes[i] >= 0) copy[keys[i]] = values[i];
            }
            return copy;
        }

        public void Remove(object key)
        {
            CheckKey(key);
            int i = FindSlot(key);
            if (i < 0) return;
            hashes[i] = Tombstone;
            keys[i] = null;
            values[i] = null;
            count = count - 1;
            version = version + 1;
        }

        public void Clear()
        {
            if (count > 0) version = version + 1;
            for (int i = 0; i < hashes.Length; i++)
            {
                keys[i] = null;
                values[i] = null;
                hashes[i] = Empty;
            }
            count = 0;
            used = 0;
        }

        public ICollection Keys
        {
            get
            {
                if (keysView == null) keysView = new HashtableKeysOrValues(this, HashtableEnumerator.Keys);
                return keysView;
            }
        }

        public ICollection Values
        {
            get
            {
                if (valuesView == null) valuesView = new HashtableKeysOrValues(this, HashtableEnumerator.Values);
                return valuesView;
            }
        }

        public IDictionaryEnumerator GetEnumerator()
        {
            return new HashtableEnumerator(this, HashtableEnumerator.Entries);
        }

        IEnumerator IEnumerable.GetEnumerator()
        {
            return GetEnumerator();
        }

        internal int NextEntry(int slot)
        {
            for (int i = slot; i < hashes.Length; i++)
            {
                if (hashes[i] >= 0) return i;
            }
            return -1;
        }

        internal object KeyAt(int slot) { return keys[slot]; }
        internal object ValueAt(int slot) { return values[slot]; }

        public void CopyTo(System.Array array, int index)
        {
            CopyTo(array, index, HashtableEnumerator.Entries);
        }

        internal void CopyTo(System.Array array, int index, int kind)
        {
            if ((object)array == null) throw new ArgumentNullException("array");
            if (array.Rank != 1) throw new ArgumentException("array");
            if (index < 0) throw new ArgumentOutOfRangeException("arrayIndex");
            if (index > array.Length - count) throw new ArgumentException();
            int n = 0;
            for (int i = 0; i < hashes.Length; i++)
            {
                if (hashes[i] < 0) continue;
                if (kind == HashtableEnumerator.Keys) array.SetValue(keys[i], index + n);
                else if (kind == HashtableEnumerator.Values) array.SetValue(values[i], index + n);
                else array.SetValue(new DictionaryEntry(keys[i], values[i]), index + n);
                n = n + 1;
            }
        }
    }
}
