// Lamella managed corlib (from scratch). -- System.Collections.HashtableKeysOrValues
namespace System.Collections
{
    internal class HashtableKeysOrValues : ICollection
    {
        private Hashtable table;
        private int kind;

        public HashtableKeysOrValues(Hashtable table, int kind)
        {
            this.table = table;
            this.kind = kind;
        }

        public int Count { get { return table.Count; } }

        public IEnumerator GetEnumerator() { return new HashtableEnumerator(table, kind); }

        public void CopyTo(System.Array array, int index) { table.CopyTo(array, index, kind); }

        public bool IsSynchronized { get { return false; } }
        public object SyncRoot { get { return table; } }
    }
}
