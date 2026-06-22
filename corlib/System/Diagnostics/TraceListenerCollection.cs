// Lamella managed corlib (from scratch). -- System.Diagnostics.TraceListenerCollection
namespace System.Diagnostics
{
    public class TraceListenerCollection
    {
        private TraceListener[] _items;
        private int _count;

        public TraceListenerCollection()
        {
            _items = new TraceListener[4];
            _count = 0;
        }

        public int Count
        {
            get { return _count; }
        }

        private void EnsureCapacity()
        {
            if (_count < _items.Length) return;
            int grown = _items.Length * 2;
            if (grown < 4) grown = 4;
            TraceListener[] bigger = new TraceListener[grown];
            for (int i = 0; i < _count; i++) bigger[i] = _items[i];
            _items = bigger;
        }

        public TraceListener this[int index]
        {
            get { return _items[index]; }
            set { _items[index] = value; }
        }

        public int Add(TraceListener listener)
        {
            EnsureCapacity();
            _items[_count] = listener;
            int at = _count;
            _count = _count + 1;
            return at;
        }

        public void Remove(TraceListener listener)
        {
            int found = -1;
            for (int i = 0; i < _count; i++)
            {
                if ((object)_items[i] == (object)listener) { found = i; break; }
            }
            if (found < 0) return;
            for (int i = found; i < _count - 1; i++) _items[i] = _items[i + 1];
            _count = _count - 1;
            _items[_count] = null;
        }

        public void Remove(string name)
        {
            int found = -1;
            for (int i = 0; i < _count; i++)
            {
                if (_items[i] != null && _items[i].Name == name) { found = i; break; }
            }
            if (found < 0) return;
            for (int i = found; i < _count - 1; i++) _items[i] = _items[i + 1];
            _count = _count - 1;
            _items[_count] = null;
        }

        public void Clear()
        {
            for (int i = 0; i < _count; i++) _items[i] = null;
            _count = 0;
        }
    }
}
