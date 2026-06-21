// Lamella managed corlib (from scratch). -- System.Collections.ObjectArrayEnumerator
namespace System.Collections
{
    internal class ObjectArrayEnumerator : IEnumerator, IDisposable
    {
        private object[] items;
        private int count;
        private int index;

        public ObjectArrayEnumerator(object[] items, int count)
        {
            this.items = items;
            this.count = count;
            this.index = -1;
        }

        public bool MoveNext()
        {
            index = index + 1;
            return index < count;
        }

        public object Current { get { return items[index]; } }

        public void Reset() { index = -1; }

        public void Dispose() { }
    }
}
