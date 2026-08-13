// Lamella managed corlib (from scratch). -- System.Collections.Generic.List<T>
#if LAMELLA_SURFACE_GENERICS
namespace System.Collections.Generic
{

    /// A dynamically sized list of `T`, backed by an array that grows by doubling.
    public class List<T> : IEnumerable
    {
        private T[] items;
        private int size;

        /// An empty list with a small initial capacity.
        public List()
        {
            items = new T[4];
            size = 0;
        }

        /// How many elements the list holds.
        public int Count { get { return size; } }

        /// How many it can hold before the backing array is replaced.
        public int Capacity { get { return items.Length; } }

        /// The element at `index`, which must be in range.
        public T this[int index]
        {
            get
            {
                if (index < 0 || index >= size) { throw new ArgumentOutOfRangeException("index"); }
                return items[index];
            }
            set
            {
                if (index < 0 || index >= size) { throw new ArgumentOutOfRangeException("index"); }
                items[index] = value;
            }
        }

        /// Appends `item`, doubling the backing array when it is full.
        public void Add(T item)
        {
            if (size == items.Length) { Grow(); }
            items[size] = item;
            size = size + 1;
        }

        /// Removes every element. The backing array is kept, and the slots it still holds are
        /// cleared so a reference type's storage does not keep an object alive past its removal.
        public void Clear()
        {
            int i = 0;
            while (i < size)
            {
                items[i] = default(T);
                i = i + 1;
            }
            size = 0;
        }

        /// The index of the first element equal to `item`, or -1.
        public int IndexOf(T item)
        {
            int i = 0;
            while (i < size)
            {
                object left = items[i];
                object right = item;
                if (left == null)
                {
                    if (right == null) { return i; }
                }
                else if (left.Equals(right))
                {
                    return i;
                }
                i = i + 1;
            }
            return -1;
        }

        /// Whether any element equals `item`.
        public bool Contains(T item)
        {
            return IndexOf(item) >= 0;
        }

        /// Removes the element at `index`, shifting the tail down.
        public void RemoveAt(int index)
        {
            if (index < 0 || index >= size) { throw new ArgumentOutOfRangeException("index"); }
            int i = index;
            while (i < size - 1)
            {
                items[i] = items[i + 1];
                i = i + 1;
            }
            size = size - 1;
            items[size] = default(T);
        }

        /// An enumerator over the elements, in order.
        public IEnumerator GetEnumerator()
        {
            object[] boxed = new object[size];
            int i = 0;
            while (i < size)
            {
                boxed[i] = items[i];
                i = i + 1;
            }
            return new ObjectArrayEnumerator(boxed, size);
        }

        private void Grow()
        {
            T[] bigger = new T[items.Length * 2];
            int i = 0;
            while (i < size)
            {
                bigger[i] = items[i];
                i = i + 1;
            }
            items = bigger;
        }
    }
}
#endif
