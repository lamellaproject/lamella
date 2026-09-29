// Lamella managed corlib (from scratch). -- System.Collections.ArrayListEnumerator
namespace System.Collections
{
    internal class ArrayListEnumerator : IEnumerator, IDisposable
    {
        private ArrayList list;
        private int index;
        private int version;
        private object current;
        private bool holding;

        public ArrayListEnumerator(ArrayList list)
        {
            this.list = list;
            this.index = -1;
            this.version = list.version;
        }

        public bool MoveNext()
        {
            CheckVersion();
            if (index < list.Count - 1)
            {
                index = index + 1;
                current = list[index];
                holding = true;
                return true;
            }
            index = list.Count;
            current = null;
            holding = false;
            return false;
        }

        public object Current
        {
            get
            {
                if (!holding)
                {
                    if (index == -1) throw new InvalidOperationException("Enumeration has not started. Call MoveNext.");
                    throw new InvalidOperationException("Enumeration already finished.");
                }
                return current;
            }
        }

        public void Reset()
        {
            CheckVersion();
            index = -1;
            current = null;
            holding = false;
        }

        private void CheckVersion()
        {
            if (version != list.version)
            {
                throw new InvalidOperationException("Collection was modified; enumeration operation may not execute.");
            }
        }

        public void Dispose() { }
    }
}
