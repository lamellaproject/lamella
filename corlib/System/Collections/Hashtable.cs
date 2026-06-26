// Lamella managed corlib (from scratch). -- System.Collections.Hashtable
namespace System.Collections
{
    public class Hashtable : IDictionary
    {
        private object[] keys;
        private object[] values;
        private int[] hashes;
        private const int Empty = -1;
        private const int Tombstone = -2;
        private int count;
        private int used;

        public Hashtable()
        {
            Initialize(8);
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

        private void Insert(object key, object value)
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
                    return;
                }
                if (state == Tombstone)
                {
                    if (tombstone < 0) tombstone = index;
                }
                else if (state == hash && keys[index] != null && keys[index].Equals(key))
                {
                    values[index] = value;
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
                Insert(key, value);
            }
        }

        public void Add(object key, object value) { this[key] = value; }

        public void Remove(object key)
        {
            CheckKey(key);
            int i = FindSlot(key);
            if (i < 0) return;
            hashes[i] = Tombstone;
            keys[i] = null;
            values[i] = null;
            count = count - 1;
        }

        public void Clear()
        {
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
                object[] snapshot = new object[count];
                int n = 0;
                for (int i = 0; i < hashes.Length; i++) if (hashes[i] >= 0) snapshot[n++] = keys[i];
                return new ObjectArrayCollection(snapshot, count);
            }
        }

        public ICollection Values
        {
            get
            {
                object[] snapshot = new object[count];
                int n = 0;
                for (int i = 0; i < hashes.Length; i++) if (hashes[i] >= 0) snapshot[n++] = values[i];
                return new ObjectArrayCollection(snapshot, count);
            }
        }

        public IEnumerator GetEnumerator()
        {
            object[] ks = new object[count];
            object[] vs = new object[count];
            int n = 0;
            for (int i = 0; i < hashes.Length; i++)
            {
                if (hashes[i] < 0) continue;
                ks[n] = keys[i];
                vs[n] = values[i];
                n = n + 1;
            }
            return new HashtableEnumerator(ks, vs, count);
        }

        public void CopyTo(System.Array array, int index)
        {
            int n = 0;
            for (int i = 0; i < hashes.Length; i++)
            {
                if (hashes[i] < 0) continue;
                array.SetValue(new DictionaryEntry(keys[i], values[i]), index + n);
                n = n + 1;
            }
        }
    }
}
