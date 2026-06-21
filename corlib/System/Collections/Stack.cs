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
            if (size == 0) throw new InvalidOperationException("Stack empty.");
            size = size - 1;
            object value = items[size];
            items[size] = null;
            return value;
        }

        public object Peek()
        {
            if (size == 0) throw new InvalidOperationException("Stack empty.");
            return items[size - 1];
        }

        public void Clear()
        {
            for (int i = 0; i < size; i++) items[i] = null;
            size = 0;
        }

        public object[] ToArray()
        {
            object[] result = new object[size];
            for (int i = 0; i < size; i++) result[i] = items[size - 1 - i];
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
    }
}
