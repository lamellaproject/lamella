// Lamella managed corlib (from scratch). -- System.Collections.ArrayList
namespace System.Collections
{
    public class ArrayList : IList
    {
        private object[] items;
        private int size;

        public ArrayList() { items = new object[4]; size = 0; }

        public int Count { get { return size; } }

        public int Capacity { get { return items.Length; } }

        public bool IsFixedSize { get { return false; } }
        public bool IsReadOnly { get { return false; } }

        public IEnumerator GetEnumerator() { return new ArrayListEnumerator(this); }

        public object this[int index]
        {
            get
            {
                if (index < 0 || index >= size) throw new ArgumentOutOfRangeException("index");
                return items[index];
            }
            set
            {
                if (index < 0 || index >= size) throw new ArgumentOutOfRangeException("index");
                items[index] = value;
            }
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

        public int IndexOf(object value)
        {
            for (int i = 0; i < size; i++)
            {
                object element = items[i];
                if (value == null)
                {
                    if (element == null) return i;
                }
                else if (element != null && element.Equals(value))
                {
                    return i;
                }
            }
            return -1;
        }

        public bool Contains(object value) { return IndexOf(value) >= 0; }

        public void Insert(int index, object value)
        {
            if (index < 0 || index > size) throw new ArgumentOutOfRangeException("index");
            if (size == items.Length)
            {
                object[] bigger = new object[items.Length * 2];
                for (int i = 0; i < size; i++) bigger[i] = items[i];
                items = bigger;
            }
            for (int i = size; i > index; i--) items[i] = items[i - 1];
            items[index] = value;
            size = size + 1;
        }

        public void RemoveAt(int index)
        {
            if (index < 0 || index >= size) throw new ArgumentOutOfRangeException("index");
            for (int i = index; i < size - 1; i++) items[i] = items[i + 1];
            size = size - 1;
            items[size] = null;
        }

        public void Remove(object value)
        {
            int i = IndexOf(value);
            if (i >= 0) RemoveAt(i);
        }

        public void Clear()
        {
            for (int i = 0; i < size; i++) items[i] = null;
            size = 0;
        }

        public void CopyTo(System.Array array, int index)
        {
            for (int i = 0; i < size; i++) array.SetValue(items[i], index + i);
        }

        public object[] ToArray()
        {
            object[] result = new object[size];
            for (int i = 0; i < size; i++) result[i] = items[i];
            return result;
        }
    }
}
