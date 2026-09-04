// Lamella managed corlib (from scratch). -- System.Collections.ArrayList
namespace System.Collections
{
    public class ArrayList : IList, ICloneable
    {
        private object[] items;
        private int size;

        private const int DefaultCapacity = 4;

        public ArrayList() { items = new object[DefaultCapacity]; size = 0; }

        public int Count { get { return size; } }

        public int Capacity
        {
            get { return items.Length; }
            set
            {
                if (value < size) throw new ArgumentOutOfRangeException("value");
                if (value == items.Length) return;
                object[] resized = new object[value > 0 ? value : DefaultCapacity];
                for (int i = 0; i < size; i++) resized[i] = items[i];
                items = resized;
            }
        }

        public bool IsFixedSize { get { return false; } }
        public bool IsReadOnly { get { return false; } }

        public bool IsSynchronized { get { return false; } }

        public object SyncRoot { get { return this; } }

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

        public int IndexOf(object value, int startIndex)
        {
            if (startIndex < 0 || startIndex > size) throw new ArgumentOutOfRangeException("startIndex");
            return IndexOf(value, startIndex, size - startIndex);
        }

        public int IndexOf(object value, int startIndex, int count)
        {
            if (startIndex < 0 || startIndex > size) throw new ArgumentOutOfRangeException("startIndex");
            if (count < 0 || startIndex > size - count) throw new ArgumentOutOfRangeException("count");
            int end = startIndex + count;
            for (int i = startIndex; i < end; i++)
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

        public int BinarySearch(object value, IComparer comparer)
        {
            if (comparer == null) comparer = Comparer.Default;
            int lo = 0;
            int hi = size - 1;
            while (lo <= hi)
            {
                int mid = lo + ((hi - lo) >> 1);
                int order = comparer.Compare(items[mid], value);
                if (order == 0) return mid;
                if (order < 0) lo = mid + 1;
                else hi = mid - 1;
            }
            return ~lo;
        }

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

        public void CopyTo(System.Array array) { CopyTo(array, 0); }

        public object Clone()
        {
            ArrayList copy = new ArrayList();
            copy.Capacity = size > 0 ? size : DefaultCapacity;
            for (int i = 0; i < size; i++) copy.Add(items[i]);
            return copy;
        }

        public object[] ToArray()
        {
            object[] result = new object[size];
            for (int i = 0; i < size; i++) result[i] = items[i];
            return result;
        }

        public System.Array ToArray(Type type)
        {
            if ((object)type == null) throw new ArgumentNullException("type");
            System.Array result = System.Array.CreateInstance(type, size);
            for (int i = 0; i < size; i++) result.SetValue(items[i], i);
            return result;
        }
    }
}
