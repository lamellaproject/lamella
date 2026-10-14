// Lamella System.Net.NetworkInformation -- System.Net.NetworkInformation.GatewayIPAddressInformationCollection
#if LAMELLA_SURFACE_NETFX_2_0
using System.Collections;
using System.Collections.Generic;

namespace System.Net.NetworkInformation
{
    public class GatewayIPAddressInformationCollection : ICollection<GatewayIPAddressInformation>
    {
        private GatewayIPAddressInformation[] _items = new GatewayIPAddressInformation[0];

        protected internal GatewayIPAddressInformationCollection() { }

        public virtual int Count { get { return _items.Length; } }

        public virtual bool IsReadOnly { get { return true; } }

        public virtual GatewayIPAddressInformation this[int index] { get { return _items[index]; } }

        public virtual void Add(GatewayIPAddressInformation address) { throw new NotSupportedException(ReadOnlyMessage); }

        public virtual void Clear() { throw new NotSupportedException(ReadOnlyMessage); }

        public virtual bool Remove(GatewayIPAddressInformation address) { throw new NotSupportedException(ReadOnlyMessage); }

        public virtual bool Contains(GatewayIPAddressInformation address)
        {
            for (int i = 0; i < _items.Length; i++)
            {
                if (object.Equals(_items[i], address)) return true;
            }
            return false;
        }

        public virtual void CopyTo(GatewayIPAddressInformation[] array, int offset)
        {
            Array.Copy(_items, 0, array, offset, _items.Length);
        }

        public virtual IEnumerator<GatewayIPAddressInformation> GetEnumerator() { return new Enumerator(_items); }

        IEnumerator IEnumerable.GetEnumerator() { return GetEnumerator(); }

        private const string ReadOnlyMessage = "The collection is read-only.";

        internal void InternalAdd(GatewayIPAddressInformation address)
        {
            GatewayIPAddressInformation[] grown = new GatewayIPAddressInformation[_items.Length + 1];
            Array.Copy(_items, grown, _items.Length);
            grown[_items.Length] = address;
            _items = grown;
        }

        private sealed class Enumerator : IEnumerator<GatewayIPAddressInformation>
        {
            private readonly GatewayIPAddressInformation[] _items;
            private int _index = -1;

            internal Enumerator(GatewayIPAddressInformation[] items) { _items = items; }

            public GatewayIPAddressInformation Current { get { return _items[_index]; } }

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
