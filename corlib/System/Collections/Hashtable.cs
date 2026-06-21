// Lamella managed corlib (from scratch). -- System.Collections.Hashtable
namespace System.Collections
{
    public class Hashtable : IDictionary
    {
        private object[] keys;
        private object[] values;
        private int size;

        public Hashtable()
        {
            keys = new object[4];
            values = new object[4];
            size = 0;
        }

        public int Count { get { return size; } }

        public bool IsFixedSize { get { return false; } }
        public bool IsReadOnly { get { return false; } }

        private int IndexOfKey(object key)
        {
            for (int i = 0; i < size; i++)
            {
                if (keys[i].Equals(key)) return i;
            }
            return -1;
        }

        private static void CheckKey(object key)
        {
            if (key == null) throw new ArgumentNullException("key");
        }

        public bool Contains(object key) { CheckKey(key); return IndexOfKey(key) >= 0; }
        public bool ContainsKey(object key) { CheckKey(key); return IndexOfKey(key) >= 0; }

        public bool ContainsValue(object value)
        {
            for (int i = 0; i < size; i++)
            {
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
                int i = IndexOfKey(key);
                if (i < 0) return null;
                return values[i];
            }
            set
            {
                CheckKey(key);
                int i = IndexOfKey(key);
                if (i >= 0) { values[i] = value; return; }
                if (size == keys.Length)
                {
                    object[] bk = new object[keys.Length * 2];
                    object[] bv = new object[values.Length * 2];
                    for (int j = 0; j < size; j++) { bk[j] = keys[j]; bv[j] = values[j]; }
                    keys = bk;
                    values = bv;
                }
                keys[size] = key;
                values[size] = value;
                size = size + 1;
            }
        }

        public void Add(object key, object value) { this[key] = value; }

        public void Remove(object key)
        {
            CheckKey(key);
            int i = IndexOfKey(key);
            if (i < 0) return;
            for (int j = i; j < size - 1; j++) { keys[j] = keys[j + 1]; values[j] = values[j + 1]; }
            size = size - 1;
            keys[size] = null;
            values[size] = null;
        }

        public void Clear()
        {
            for (int i = 0; i < size; i++) { keys[i] = null; values[i] = null; }
            size = 0;
        }

        public ICollection Keys
        {
            get
            {
                object[] snapshot = new object[size];
                for (int i = 0; i < size; i++) snapshot[i] = keys[i];
                return new ObjectArrayCollection(snapshot, size);
            }
        }

        public ICollection Values
        {
            get
            {
                object[] snapshot = new object[size];
                for (int i = 0; i < size; i++) snapshot[i] = values[i];
                return new ObjectArrayCollection(snapshot, size);
            }
        }

        public IEnumerator GetEnumerator()
        {
            object[] ks = new object[size];
            object[] vs = new object[size];
            for (int i = 0; i < size; i++) { ks[i] = keys[i]; vs[i] = values[i]; }
            return new HashtableEnumerator(ks, vs, size);
        }

        public void CopyTo(System.Array array, int index)
        {
            for (int i = 0; i < size; i++) array.SetValue(new DictionaryEntry(keys[i], values[i]), index + i);
        }
    }
}
