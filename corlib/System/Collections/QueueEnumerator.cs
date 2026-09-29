// Lamella managed corlib (from scratch). -- System.Collections.QueueEnumerator
namespace System.Collections
{
    internal class QueueEnumerator : IEnumerator, IDisposable
    {
        private Queue queue;
        private int index;
        private int version;
        private object current;
        private bool holding;

        public QueueEnumerator(Queue queue)
        {
            this.queue = queue;
            this.version = queue.version;
            this.index = queue.Count == 0 ? -1 : 0;
        }

        public bool MoveNext()
        {
            CheckVersion();
            if (index < 0)
            {
                current = null;
                holding = false;
                return false;
            }
            current = queue.GetElement(index);
            holding = true;
            index = index + 1;
            if (index == queue.Count) index = -1;
            return true;
        }

        public object Current
        {
            get
            {
                if (!holding)
                {
                    if (index == 0) throw new InvalidOperationException("Enumeration has not started. Call MoveNext.");
                    throw new InvalidOperationException("Enumeration already finished.");
                }
                return current;
            }
        }

        public void Reset()
        {
            CheckVersion();
            index = queue.Count == 0 ? -1 : 0;
            current = null;
            holding = false;
        }

        private void CheckVersion()
        {
            if (version != queue.version)
            {
                throw new InvalidOperationException("Collection was modified after the enumerator was instantiated.");
            }
        }

        public void Dispose() { }
    }
}
