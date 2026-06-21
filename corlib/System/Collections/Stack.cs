// Lamella managed corlib (from scratch). -- System.Collections.Stack
namespace System.Collections
{
    public class Stack : IEnumerable
    {
        private object[] items;
        private int size;
        public Stack() { items = new object[4]; size = 0; }
        public int Count { get { return size; } }

        public IEnumerator GetEnumerator() { return new StackEnumerator(this); }
        internal object GetElement(int index) { return items[index]; }
        public void Push(object value)
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
        public object Pop()
        {
            size = size - 1;
            object value = items[size];
            items[size] = null;
            return value;
        }
        public object Peek() { return items[size - 1]; }
    }
}
