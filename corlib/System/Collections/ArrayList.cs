// Lamella managed corlib (from scratch). -- System.Collections.ArrayList
namespace System.Collections
{
    public class ArrayList : IEnumerable
    {
        private object[] items;
        private int size;

        public ArrayList() { items = new object[4]; size = 0; }

        public int Count { get { return size; } }

        public IEnumerator GetEnumerator() { return new ArrayListEnumerator(this); }

        public object this[int index]
        {
            get { return items[index]; }
            set { items[index] = value; }
        }

        public int Add(object value)
        {
            if (size == items.Length)
            {
                object[] bigger = new object[items.Length * 2];
                for (int i = 0; i < size; i++) bigger[i] = items[i];
                items = bigger;
            }
            items[size] = value;
            size = size + 1;
            return size - 1;
        }
    }
}
