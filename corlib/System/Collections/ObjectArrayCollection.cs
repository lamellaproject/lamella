// Lamella managed corlib (from scratch). -- System.Collections.ObjectArrayCollection
namespace System.Collections
{
    internal class ObjectArrayCollection : ICollection
    {
        private object[] items;
        private int count;
        private object owner;

        public ObjectArrayCollection(object[] items, int count, object owner)
        {
            this.items = items;
            this.count = count;
            this.owner = owner;
        }

        public int Count { get { return count; } }

        public IEnumerator GetEnumerator() { return new ObjectArrayEnumerator(items, count); }

        public void CopyTo(System.Array array, int index)
        {
            for (int i = 0; i < count; i++) array.SetValue(items[i], index + i);
        }

        public bool IsSynchronized { get { return false; } }
        public object SyncRoot { get { return owner; } }
    }
}
