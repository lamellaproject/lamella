// Lamella managed corlib (from scratch). -- System.Collections.Generic.List<T>
#if LAMELLA_SURFACE_NETFX_2_0
namespace System.Collections.Generic
{

    /// A dynamically sized list of `T`, backed by an array that grows by doubling.
    public class List<T> : IEnumerable<T>, IEnumerable, IEnumerationVersion
    {
        private T[] items;
        private int size;
        private const string IndexMustBeLess = "Index was out of range. Must be non-negative and less than the size of the collection.";
        private const string IndexMustBeLessOrEqual = "Index was out of range. Must be non-negative and less than or equal to the size of the collection.";
        private const string NeedNonNegNum = "Non-negative number required.";
        private const string InvalidOffLen = "Offset and length were out of bounds for the array or count is greater than the number of elements from index to the end of the source collection.";
        private const string CountOutOfRange = "Count must be positive and count must refer to a location within the string/array/collection.";
        private const string BiggerThanCollection = "Must be less than or equal to the size of the collection.";
        private int version;

        /// An empty list.
        public List()
        {
            items = new T[0];
            size = 0;
        }

        /// An empty list with room for `capacity` elements.
        public List(int capacity)
        {
            if (capacity < 0) { throw new ArgumentOutOfRangeException("capacity", NeedNonNegNum); }
            items = new T[capacity];
            size = 0;
        }

        /// How many elements the list holds.
        public int Count { get { return size; } }

        /// How many elements it can hold before the backing array is replaced.
        public int Capacity
        {
            get { return items.Length; }
            set
            {
                if (value < size) { throw new ArgumentOutOfRangeException("value", "capacity was less than the current size."); }
                Resize(value);
            }
        }

        private void Resize(int capacity)
        {
            if (capacity != items.Length)
            {
                T[] resized = new T[capacity];
                int i = 0;
                while (i < size)
                {
                    resized[i] = items[i];
                    i = i + 1;
                }
                items = resized;
            }
        }

        /// The element at `index`, which must be in range.
        public T this[int index]
        {
            get
            {
                if (index < 0 || index >= size) { throw new ArgumentOutOfRangeException("index", IndexMustBeLess); }
                return items[index];
            }
            set
            {
                if (index < 0 || index >= size) { throw new ArgumentOutOfRangeException("index", IndexMustBeLess); }
                items[index] = value;
                version = version + 1;
            }
        }

        /// Appends `item`, doubling the backing array when it is full.
        public void Add(T item)
        {
            if (size == items.Length) { Grow(size + 1); }
            items[size] = item;
            size = size + 1;
            version = version + 1;
        }

        /// Inserts `item` at `index`, which may be Count; the elements from `index` move up one.
        public void Insert(int index, T item)
        {
            if (index < 0 || index > size) { throw new ArgumentOutOfRangeException("index", "Index must be within the bounds of the List."); }
            if (size == items.Length) { Grow(size + 1); }
            int i = size;
            while (i > index)
            {
                items[i] = items[i - 1];
                i = i - 1;
            }
            items[index] = item;
            size = size + 1;
            version = version + 1;
        }

        /// Removes every element. The backing array is kept, and the slots it still holds are
        /// cleared so a reference type's storage does not keep an object alive past its removal.
        public void Clear()
        {
            version = version + 1;
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
            return Find(item, 0, size);
        }

        /// The index of the first element equal to `item` from `index` on, or -1.
        public int IndexOf(T item, int index)
        {
            if (index > size) { throw new ArgumentOutOfRangeException("index", IndexMustBeLessOrEqual); }
            return Find(item, index, size - index);
        }

        /// The index of the first element equal to `item` among `count` from `index`, or -1.
        public int IndexOf(T item, int index, int count)
        {
            if (index > size) { throw new ArgumentOutOfRangeException("index", IndexMustBeLessOrEqual); }
            if (count < 0 || index > size - count) { throw new ArgumentOutOfRangeException("count", CountOutOfRange); }
            return Find(item, index, count);
        }

        /// The index of the last element equal to `item`, or -1.
        public int LastIndexOf(T item)
        {
            if (size == 0) { return -1; }
            return LastIndexOf(item, size - 1, size);
        }

        /// The index of the last element equal to `item` at or before `index`, or -1.
        public int LastIndexOf(T item, int index)
        {
            if (index >= size) { throw new ArgumentOutOfRangeException("index", IndexMustBeLess); }
            return LastIndexOf(item, index, index + 1);
        }

        /// The index of the last element equal to `item` among `count` ending at `index`, or -1.
        public int LastIndexOf(T item, int index, int count)
        {
            if (size != 0 && index < 0) { throw new ArgumentOutOfRangeException("index", NeedNonNegNum); }
            if (size != 0 && count < 0) { throw new ArgumentOutOfRangeException("count", NeedNonNegNum); }
            if (size == 0) { return -1; }
            if (index >= size) { throw new ArgumentOutOfRangeException("index", BiggerThanCollection); }
            if (count > index + 1) { throw new ArgumentOutOfRangeException("count", BiggerThanCollection); }
            int i = index;
            int end = index - count;
            while (i > end)
            {
                if (Matches(items[i], item)) { return i; }
                i = i - 1;
            }
            return -1;
        }

        private int Find(T item, int start, int count)
        {
            if (start < 0) { throw new ArgumentOutOfRangeException("startIndex", IndexMustBeLessOrEqual); }
            int i = start;
            int end = start + count;
            while (i < end)
            {
                if (Matches(items[i], item)) { return i; }
                i = i + 1;
            }
            return -1;
        }

        private static bool Matches(T element, T item)
        {
            object left = element;
            object right = item;
            if (left == null) { return right == null; }
            return right != null && left.Equals(right);
        }

        /// Whether any element equals `item`.
        public bool Contains(T item)
        {
            return IndexOf(item) >= 0;
        }

        /// Removes the first element equal to `item`, answering whether there was one.
        public bool Remove(T item)
        {
            int index = IndexOf(item);
            if (index < 0) { return false; }
            RemoveAt(index);
            return true;
        }

        /// Removes the element at `index`, shifting the tail down.
        public void RemoveAt(int index)
        {
            if (index < 0 || index >= size) { throw new ArgumentOutOfRangeException("index", IndexMustBeLess); }
            int i = index;
            while (i < size - 1)
            {
                items[i] = items[i + 1];
                i = i + 1;
            }
            size = size - 1;
            items[size] = default(T);
            version = version + 1;
        }

        /// Whether any element satisfies `match`.
        public bool Exists(Predicate<T> match)
        {
            return FindIndex(0, size, match) != -1;
        }

        /// The first element that satisfies `match`, or default(T).
        public T Find(Predicate<T> match)
        {
            if (match == null) { throw new ArgumentNullException("match"); }
            int i = 0;
            while (i < size)
            {
                if (match(items[i])) { return items[i]; }
                i = i + 1;
            }
            return default(T);
        }

        /// A new list of every element that satisfies `match`, in order.
        public List<T> FindAll(Predicate<T> match)
        {
            if (match == null) { throw new ArgumentNullException("match"); }
            List<T> found = new List<T>();
            int i = 0;
            while (i < size)
            {
                if (match(items[i])) { found.Add(items[i]); }
                i = i + 1;
            }
            return found;
        }

        /// The index of the first element that satisfies `match`, or -1.
        public int FindIndex(Predicate<T> match)
        {
            return FindIndex(0, size, match);
        }

        /// The index of the first element from `startIndex` on that satisfies `match`, or -1.
        public int FindIndex(int startIndex, Predicate<T> match)
        {
            return FindIndex(startIndex, size - startIndex, match);
        }

        /// The index of the first of `count` elements from `startIndex` that satisfies `match`, or -1.
        public int FindIndex(int startIndex, int count, Predicate<T> match)
        {
            if (startIndex < 0 || startIndex > size) { throw new ArgumentOutOfRangeException("startIndex", IndexMustBeLessOrEqual); }
            if (count < 0 || startIndex > size - count) { throw new ArgumentOutOfRangeException("count", CountOutOfRange); }
            if (match == null) { throw new ArgumentNullException("match"); }
            int i = startIndex;
            int end = startIndex + count;
            while (i < end)
            {
                if (match(items[i])) { return i; }
                i = i + 1;
            }
            return -1;
        }

        /// The last element that satisfies `match`, or default(T).
        public T FindLast(Predicate<T> match)
        {
            if (match == null) { throw new ArgumentNullException("match"); }
            int i = size - 1;
            while (i >= 0)
            {
                if (match(items[i])) { return items[i]; }
                i = i - 1;
            }
            return default(T);
        }

        /// The index of the last element that satisfies `match`, or -1.
        public int FindLastIndex(Predicate<T> match)
        {
            return FindLastIndex(size - 1, size, match);
        }

        /// The index of the last element at or before `startIndex` that satisfies `match`, or -1.
        public int FindLastIndex(int startIndex, Predicate<T> match)
        {
            return FindLastIndex(startIndex, startIndex + 1, match);
        }

        /// The index of the last of `count` elements ending at `startIndex` that satisfies `match`,
        /// or -1.
        public int FindLastIndex(int startIndex, int count, Predicate<T> match)
        {
            if (match == null) { throw new ArgumentNullException("match"); }
            if (size == 0)
            {
                if (startIndex != -1) { throw new ArgumentOutOfRangeException("startIndex", IndexMustBeLess); }
            }
            else if (startIndex < 0 || startIndex >= size)
            {
                throw new ArgumentOutOfRangeException("startIndex", IndexMustBeLess);
            }
            if (count < 0 || startIndex - count + 1 < 0) { throw new ArgumentOutOfRangeException("count", CountOutOfRange); }
            int i = startIndex;
            int end = startIndex - count;
            while (i > end)
            {
                if (match(items[i])) { return i; }
                i = i - 1;
            }
            return -1;
        }

        /// Runs `action` on each element in order.
        public void ForEach(Action<T> action)
        {
            if (action == null) { throw new ArgumentNullException("action"); }
            int start = version;
            int i = 0;
            while (i < size)
            {
                if (start != version) { break; }
                action(items[i]);
                i = i + 1;
            }
            if (start != version) { throw new InvalidOperationException("Collection was modified; enumeration operation may not execute."); }
        }

        /// Whether every element satisfies `match`.
        public bool TrueForAll(Predicate<T> match)
        {
            if (match == null) { throw new ArgumentNullException("match"); }
            int i = 0;
            while (i < size)
            {
                if (!match(items[i])) { return false; }
                i = i + 1;
            }
            return true;
        }

        /// Removes every element that satisfies `match`, answering how many went.
        public int RemoveAll(Predicate<T> match)
        {
            if (match == null) { throw new ArgumentNullException("match"); }
            int free = 0;
            while (free < size && !match(items[free])) { free = free + 1; }
            if (free >= size) { return 0; }
            int current = free + 1;
            while (current < size)
            {
                while (current < size && match(items[current])) { current = current + 1; }
                if (current < size)
                {
                    items[free] = items[current];
                    free = free + 1;
                    current = current + 1;
                }
            }
            int removed = size - free;
            int i = free;
            while (i < size)
            {
                items[i] = default(T);
                i = i + 1;
            }
            size = free;
            version = version + 1;
            return removed;
        }

        /// Removes `count` elements from `index` on.
        public void RemoveRange(int index, int count)
        {
            CheckRange(index, count);
            if (count > 0)
            {
                int i = index;
                while (i + count < size)
                {
                    items[i] = items[i + count];
                    i = i + 1;
                }
                size = size - count;
                i = size;
                while (i < size + count)
                {
                    items[i] = default(T);
                    i = i + 1;
                }
                version = version + 1;
            }
        }

        /// Reverses the order of the elements.
        public void Reverse()
        {
            Reverse(0, size);
        }

        /// Reverses the order of `count` elements from `index` on.
        public void Reverse(int index, int count)
        {
            CheckRange(index, count);
            int low = index;
            int high = index + count - 1;
            while (low < high)
            {
                T swap = items[low];
                items[low] = items[high];
                items[high] = swap;
                low = low + 1;
                high = high - 1;
            }
            version = version + 1;
        }

        /// A new list of `count` elements copied from `index` on.
        public List<T> GetRange(int index, int count)
        {
            CheckRange(index, count);
            List<T> range = new List<T>(count);
            int i = 0;
            while (i < count)
            {
                range.items[i] = items[index + i];
                i = i + 1;
            }
            range.size = count;
            return range;
        }

        /// A new array holding the elements in order.
        public T[] ToArray()
        {
            T[] array = new T[size];
            int i = 0;
            while (i < size)
            {
                array[i] = items[i];
                i = i + 1;
            }
            return array;
        }

        /// Copies the elements into `array` from its start.
        public void CopyTo(T[] array)
        {
            CopyTo(array, 0);
        }

        /// Copies the elements into `array` from `arrayIndex` on.
        public void CopyTo(T[] array, int arrayIndex)
        {
            Array.Copy(items, 0, array, arrayIndex, size);
        }

        /// Copies `count` elements from `index` on into `array` from `arrayIndex` on.
        public void CopyTo(int index, T[] array, int arrayIndex, int count)
        {
            if (size - index < count) { throw new ArgumentException(InvalidOffLen); }
            Array.Copy(items, index, array, arrayIndex, count);
        }

        /// Sets the capacity to Count, if that saves more than a tenth of it.
        public void TrimExcess()
        {
            int threshold = (int)((long)items.Length * 9 / 10);
            if (size < threshold) { Resize(size); }
        }

        private void CheckRange(int index, int count)
        {
            if (index < 0) { throw new ArgumentOutOfRangeException("index", NeedNonNegNum); }
            if (count < 0) { throw new ArgumentOutOfRangeException("count", NeedNonNegNum); }
            if (size - index < count) { throw new ArgumentException(InvalidOffLen); }
        }

        /// An enumerator over the elements, in order.
        public IEnumerator<T> GetEnumerator()
        {
            return new ListEnumerator<T>(this, items, size);
        }

        IEnumerator IEnumerable.GetEnumerator()
        {
            return new ListEnumerator<T>(this, items, size);
        }

        int IEnumerationVersion.EnumerationVersion
        {
            get { return version; }
        }

        private void Grow(int min)
        {
            int capacity = items.Length == 0 ? 4 : items.Length * 2;
            if (capacity < min) { capacity = min; }
            Resize(capacity);
        }
    }
}
#endif
