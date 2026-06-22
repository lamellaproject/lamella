// Lamella managed corlib (from scratch). -- System.Collections.SortedList
namespace System.Collections
{
    public class SortedList : IDictionary
    {
        private object[] keys;
        private object[] values;
        private int size;
        private IComparer comparer;

        public SortedList()
        {
            keys = new object[16];
            values = new object[16];
            size = 0;
            comparer = null;
        }

        public SortedList(IComparer comparer)
        {
            keys = new object[16];
            values = new object[16];
            size = 0;
            this.comparer = comparer;
        }

        public SortedList(int capacity)
        {
            if (capacity < 0) throw new ArgumentOutOfRangeException("capacity");
            keys = new object[capacity];
            values = new object[capacity];
            size = 0;
            comparer = null;
        }

        public int Count { get { return size; } }

        public int Capacity { get { return keys.Length; } }

        public bool IsFixedSize { get { return false; } }
        public bool IsReadOnly { get { return false; } }

        private IComparer EffectiveComparer()
        {
            if (comparer == null) return Comparer.Default;
            return comparer;
        }

        private static void CheckKey(object key)
        {
            if (key == null) throw new ArgumentNullException("key");
        }

        private int InternalIndexOfKey(object key)
        {
            IComparer c = EffectiveComparer();
            int lo = 0;
            int hi = size - 1;
            while (lo <= hi)
            {
                int mid = lo + ((hi - lo) >> 1);
                int order = c.Compare(keys[mid], key);
                if (order == 0) return mid;
                if (order < 0) lo = mid + 1;
                else hi = mid - 1;
            }
            return ~lo;
        }

        public int IndexOfKey(object key)
        {
            CheckKey(key);
            int i = InternalIndexOfKey(key);
            if (i < 0) return -1;
            return i;
        }

        public int IndexOfValue(object value)
        {
            for (int i = 0; i < size; i++)
            {
                object v = values[i];
                if (value == null) { if (v == null) return i; }
                else if (v != null && v.Equals(value)) return i;
            }
            return -1;
        }

        public bool Contains(object key) { return ContainsKey(key); }

        public bool ContainsKey(object key)
        {
            CheckKey(key);
            return InternalIndexOfKey(key) >= 0;
        }

        public bool ContainsValue(object value)
        {
            return IndexOfValue(value) >= 0;
        }

        private void EnsureCapacity(int min)
        {
            if (keys.Length >= min) return;
            int next = keys.Length * 2;
            if (next < 16) next = 16;
            if (next < min) next = min;
            object[] bk = new object[next];
            object[] bv = new object[next];
            for (int i = 0; i < size; i++) { bk[i] = keys[i]; bv[i] = values[i]; }
            keys = bk;
            values = bv;
        }

        private void InsertAt(int index, object key, object value)
        {
            EnsureCapacity(size + 1);
            for (int i = size; i > index; i--)
            {
                keys[i] = keys[i - 1];
                values[i] = values[i - 1];
            }
            keys[index] = key;
            values[index] = value;
            size = size + 1;
        }

        public void Add(object key, object value)
        {
            CheckKey(key);
            int i = InternalIndexOfKey(key);
            if (i >= 0) throw new ArgumentException("Item has already been added. Key in dictionary already exists.");
            InsertAt(~i, key, value);
        }

        public object this[object key]
        {
            get
            {
                CheckKey(key);
                int i = InternalIndexOfKey(key);
                if (i < 0) return null;
                return values[i];
            }
            set
            {
                CheckKey(key);
                int i = InternalIndexOfKey(key);
                if (i >= 0) { values[i] = value; return; }
                InsertAt(~i, key, value);
            }
        }

        public object GetByIndex(int index)
        {
            if (index < 0 || index >= size) throw new ArgumentOutOfRangeException("index");
            return values[index];
        }

        public object GetKey(int index)
        {
            if (index < 0 || index >= size) throw new ArgumentOutOfRangeException("index");
            return keys[index];
        }

        public void SetByIndex(int index, object value)
        {
            if (index < 0 || index >= size) throw new ArgumentOutOfRangeException("index");
            values[index] = value;
        }

        public void RemoveAt(int index)
        {
            if (index < 0 || index >= size) throw new ArgumentOutOfRangeException("index");
            for (int i = index; i < size - 1; i++)
            {
                keys[i] = keys[i + 1];
                values[i] = values[i + 1];
            }
            size = size - 1;
            keys[size] = null;
            values[size] = null;
        }

        public void Remove(object key)
        {
            CheckKey(key);
            int i = InternalIndexOfKey(key);
            if (i >= 0) RemoveAt(i);
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
            return new SortedListEnumerator(ks, vs, size);
        }

        public void CopyTo(System.Array array, int index)
        {
            for (int i = 0; i < size; i++) array.SetValue(new DictionaryEntry(keys[i], values[i]), index + i);
        }
    }
}
