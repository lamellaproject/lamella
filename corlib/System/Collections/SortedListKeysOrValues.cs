// Lamella managed corlib (from scratch). -- System.Collections.SortedListKeysOrValues
namespace System.Collections
{
    internal class SortedListKeysOrValues : ICollection
    {
        private SortedList list;
        private int kind;

        public SortedListKeysOrValues(SortedList list, int kind)
        {
            this.list = list;
            this.kind = kind;
        }

        public int Count { get { return list.Count; } }

        public IEnumerator GetEnumerator() { return new SortedListEnumerator(list, kind); }

        public void CopyTo(System.Array array, int index) { list.CopyTo(array, index, kind); }

        public bool IsSynchronized { get { return false; } }
        public object SyncRoot { get { return list; } }
    }
}
