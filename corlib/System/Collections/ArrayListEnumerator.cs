// Lamella managed corlib (from scratch). -- System.Collections.ArrayListEnumerator
namespace System.Collections
{
    internal class ArrayListEnumerator : IEnumerator, IDisposable
    {
        private ArrayList list;
        private int index;

        public ArrayListEnumerator(ArrayList list)
        {
            this.list = list;
            this.index = -1;
        }

        public bool MoveNext()
        {
            index = index + 1;
            return index < list.Count;
        }

        public object Current
        {
            get { return list[index]; }
        }

        public void Reset() { index = -1; }

        public void Dispose() { }
    }
}
