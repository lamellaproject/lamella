// Lamella managed corlib (from scratch). -- System.Collections.Queue
namespace System.Collections
{
    public class Queue : ICollection, ICloneable
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
            if (size == 0) throw new InvalidOperationException("Queue empty.");
            object value = items[0];
            for (int i = 1; i < size; i++) items[i - 1] = items[i];
            size = size - 1;
            items[size] = null;
            return value;
        }

        public object Peek()
        {
            if (size == 0) throw new InvalidOperationException("Queue empty.");
            return items[0];
        }

        public void Clear()
        {
            for (int i = 0; i < size; i++) items[i] = null;
            size = 0;
        }

        public object[] ToArray()
        {
            object[] result = new object[size];
            for (int i = 0; i < size; i++) result[i] = items[i];
            return result;
        }

        public bool Contains(object value)
        {
            for (int i = 0; i < size; i++)
            {
                object item = items[i];
                if (value == null) { if (item == null) return true; }
                else if (item != null && item.Equals(value)) return true;
            }
            return false;
        }

        /// <summary>Copies the elements, front first, into <paramref name="array"/> at <paramref name="index"/>.</summary>
        public void CopyTo(System.Array array, int index)
        {
            if ((object)array == null) throw new ArgumentNullException("array");
            if (array.Rank != 1) throw new ArgumentException("array");
            if (index < 0) throw new ArgumentOutOfRangeException("index");
            if (index > array.Length - size) throw new ArgumentException();
            for (int i = 0; i < size; i++) array.SetValue(items[i], index + i);
        }

        public bool IsSynchronized { get { return false; } }
        public object SyncRoot { get { return this; } }

        public object Clone()
        {
            Queue copy = new Queue();
            copy.items = new object[size > 0 ? size : 4];
            for (int i = 0; i < size; i++) copy.items[i] = items[i];
            copy.size = size;
            return copy;
        }
    }
}
