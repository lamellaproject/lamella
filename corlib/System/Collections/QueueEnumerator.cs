// Lamella managed corlib (from scratch). -- System.Collections.QueueEnumerator
namespace System.Collections
{
    internal class QueueEnumerator : IEnumerator, IDisposable
    {
        private Queue queue;
        private int index;

        public QueueEnumerator(Queue queue)
        {
            this.queue = queue;
            this.index = -1;
        }

        public bool MoveNext()
        {
            index = index + 1;
            return index < queue.Count;
        }

        public object Current
        {
            get { return queue.GetElement(index); }
        }

        public void Reset() { index = -1; }

        public void Dispose() { }
    }
}
