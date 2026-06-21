// Lamella managed corlib (from scratch). -- System.Collections.Hashtable
namespace System.Collections
{
    public class Hashtable
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

        private int IndexOfKey(object key)
        {
            for (int i = 0; i < size; i++)
            {
                if (keys[i].Equals(key)) return i;
            }
            return -1;
        }

        public bool Contains(object key) { return IndexOfKey(key) >= 0; }
        public bool ContainsKey(object key) { return IndexOfKey(key) >= 0; }

        public object this[object key]
        {
            get
            {
                int i = IndexOfKey(key);
                if (i < 0) return null;
                return values[i];
            }
            set
            {
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
    }
}
