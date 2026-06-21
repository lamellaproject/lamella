// Lamella managed corlib (from scratch). -- System.Collections.Queue
namespace System.Collections
{
    public class Queue : IEnumerable
    {
        private object[] items;
        private int size;
        public Queue() { items = new object[4]; size = 0; }
        public int Count { get { return size; } }

        public IEnumerator GetEnumerator() { return new QueueEnumerator(this); }
        internal object GetElement(int index) { return items[index]; }
        public void Enqueue(object value)
        {
            if (size == items.Length)
            {
                object[] bigger = new object[items.Length * 2];
                for (int i = 0; i < size; i++) bigger[i] = items[i];
                items = bigger;
            }
            items[size] = value;
            size = size + 1;
        }
        public object Dequeue()
        {
            object value = items[0];
            for (int i = 1; i < size; i++) items[i - 1] = items[i];
            size = size - 1;
            items[size] = null;
            return value;
        }
        public object Peek() { return items[0]; }
    }
}
