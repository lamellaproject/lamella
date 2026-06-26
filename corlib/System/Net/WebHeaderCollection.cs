// Lamella managed corlib (from scratch). -- System.Net.WebHeaderCollection
#if LAMELLA_SURFACE_NET
namespace System.Net
{
    public class WebHeaderCollection
    {
        private string[] _keys;
        private string[] _values;
        private int _count;

        public WebHeaderCollection()
        {
            _keys = new string[8];
            _values = new string[8];
            _count = 0;
        }

        private int IndexOfKey(string name)
        {
            for (int i = 0; i < _count; i++)
                if (EqualsIgnoreCase(_keys[i], name)) return i;
            return -1;
        }

        private static bool EqualsIgnoreCase(string a, string b)
        {
            if ((object)a == null || (object)b == null) return (object)a == (object)b;
            if (a.Length != b.Length) return false;
            return a.ToLower() == b.ToLower();
        }

        private void EnsureCapacity(int needed)
        {
            if (needed <= _keys.Length) return;
            int cap = _keys.Length * 2;
            if (cap < needed) cap = needed;
            string[] nk = new string[cap];
            string[] nv = new string[cap];
            for (int i = 0; i < _count; i++) { nk[i] = _keys[i]; nv[i] = _values[i]; }
            _keys = nk;
            _values = nv;
        }

        public void Add(string name, string value)
        {
            if ((object)name == null) throw new ArgumentNullException("name");
            int idx = IndexOfKey(name);
            if (idx >= 0)
            {
                _values[idx] = _values[idx] + "," + value;
            }
            else
            {
                EnsureCapacity(_count + 1);
                _keys[_count] = name;
                _values[_count] = value;
                _count++;
            }
        }

        public void Set(string name, string value)
        {
            if ((object)name == null) throw new ArgumentNullException("name");
            int idx = IndexOfKey(name);
            if (idx >= 0)
            {
                _values[idx] = value;
            }
            else
            {
                EnsureCapacity(_count + 1);
                _keys[_count] = name;
                _values[_count] = value;
                _count++;
            }
        }

        public void Remove(string name)
        {
            int idx = IndexOfKey(name);
            if (idx < 0) return;
            for (int i = idx; i < _count - 1; i++)
            {
                _keys[i] = _keys[i + 1];
                _values[i] = _values[i + 1];
            }
            _count--;
            _keys[_count] = null;
            _values[_count] = null;
        }

        public string this[string name]
        {
            get
            {
                int idx = IndexOfKey(name);
                return idx >= 0 ? _values[idx] : null;
            }
            set { Set(name, value); }
        }

        public int Count { get { return _count; } }

        public string GetKey(int index)
        {
            if (index < 0 || index >= _count) throw new ArgumentOutOfRangeException("index");
            return _keys[index];
        }

        public string Get(int index)
        {
            if (index < 0 || index >= _count) throw new ArgumentOutOfRangeException("index");
            return _values[index];
        }

        public string Get(string name) { return this[name]; }

        public string[] AllKeys
        {
            get
            {
                string[] result = new string[_count];
                for (int i = 0; i < _count; i++) result[i] = _keys[i];
                return result;
            }
        }
    }
}
#endif
