// Lamella System.Net.NetworkInformation -- System.Net.NetworkInformation.UnicastIPAddressInformationCollection
#if LAMELLA_SURFACE_NETFX_2_0
using System.Collections;
using System.Collections.Generic;

namespace System.Net.NetworkInformation
{
    public class UnicastIPAddressInformationCollection : ICollection<UnicastIPAddressInformation>
    {
        private UnicastIPAddressInformation[] _items = new UnicastIPAddressInformation[0];

        protected internal UnicastIPAddressInformationCollection() { }

        public virtual int Count { get { return _items.Length; } }

        public virtual bool IsReadOnly { get { return true; } }

        public virtual UnicastIPAddressInformation this[int index] { get { return _items[index]; } }

        public virtual void Add(UnicastIPAddressInformation address) { throw new NotSupportedException(ReadOnlyMessage); }

        public virtual void Clear() { throw new NotSupportedException(ReadOnlyMessage); }

        public virtual bool Remove(UnicastIPAddressInformation address) { throw new NotSupportedException(ReadOnlyMessage); }

        public virtual bool Contains(UnicastIPAddressInformation address)
        {
            for (int i = 0; i < _items.Length; i++)
            {
                if (object.Equals(_items[i], address)) return true;
            }
            return false;
        }

        public virtual void CopyTo(UnicastIPAddressInformation[] array, int offset)
        {
            Array.Copy(_items, 0, array, offset, _items.Length);
        }

        public virtual IEnumerator<UnicastIPAddressInformation> GetEnumerator() { return new Enumerator(_items); }

        IEnumerator IEnumerable.GetEnumerator() { return GetEnumerator(); }

        private const string ReadOnlyMessage = "The collection is read-only.";

        internal void InternalAdd(UnicastIPAddressInformation address)
        {
            UnicastIPAddressInformation[] grown = new UnicastIPAddressInformation[_items.Length + 1];
            Array.Copy(_items, grown, _items.Length);
            grown[_items.Length] = address;
            _items = grown;
        }

        private sealed class Enumerator : IEnumerator<UnicastIPAddressInformation>
        {
            private readonly UnicastIPAddressInformation[] _items;
            private int _index = -1;

            internal Enumerator(UnicastIPAddressInformation[] items) { _items = items; }

            public UnicastIPAddressInformation Current { get { return _items[_index]; } }

            object IEnumerator.Current { get { return _items[_index]; } }

            public bool MoveNext()
            {
                if (_index + 1 >= _items.Length) return false;
                _index++;
                return true;
            }

            public void Reset() { _index = -1; }

            public void Dispose() { }
        }
    }
}
#endif
