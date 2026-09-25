// Lamella managed corlib (from scratch). -- System.Collections.Generic.Queue<T>
#if LAMELLA_SURFACE_NETFX_2_0
namespace System.Collections.Generic
{

    /// <summary>A first-in, first-out collection of <typeparamref name="T"/>.</summary>
    /// <typeparam name="T">The type of the elements. An element can be null when the type allows it.</typeparam>
    public class Queue<T> : IEnumerable<T>, ICollection, IEnumerable
    {
        private T[] items;
        private int head;
        private int count;
        private int version;

        /// <summary>An empty queue.</summary>
        public Queue()
        {
        }

        /// <summary>An empty queue with room for <paramref name="capacity"/> elements before it grows.</summary>
        /// <param name="capacity">How many elements to make room for.</param>
        /// <exception cref="ArgumentOutOfRangeException"><paramref name="capacity"/> is negative.</exception>
        public Queue(int capacity)
        {
            if (capacity < 0) throw new ArgumentOutOfRangeException("capacity");
            if (capacity > 0) items = new T[capacity];
        }

        /// <summary>A queue holding the elements of <paramref name="collection"/>, the first one enumerated at the head.</summary>
        /// <param name="collection">The elements to copy.</param>
        /// <exception cref="ArgumentNullException"><paramref name="collection"/> is null.</exception>
        public Queue(IEnumerable<T> collection)
        {
            if (collection == null) throw new ArgumentNullException("collection");
            T[] array = collection as T[];
            if (array != null)
            {
                if (array.Length > 0)
                {
                    items = new T[array.Length];
                    for (int i = 0; i < array.Length; i++) items[i] = array[i];
                    count = array.Length;
                }
                return;
            }
            foreach (T item in collection)
            {
                Enqueue(item);
            }
        }

        /// <summary>How many elements the queue holds.</summary>
        public int Count
        {
            get { return count; }
        }

        /// <summary>Removes every element. The storage the queue has already made is kept.</summary>
        public void Clear()
        {
            for (int i = 0; i < count; i++)
            {
                items[SlotOf(i)] = default(T);
            }
            head = 0;
            count = 0;
            version = version + 1;
        }

        /// <summary>Whether the queue holds an element equal to <paramref name="item"/>, by the default equality comparer for <typeparamref name="T"/>.</summary>
        /// <param name="item">The element to look for; null matches a null element.</param>
        /// <returns>True when a matching element is present.</returns>
        public bool Contains(T item)
        {
            EqualityComparer<T> comparer = EqualityComparer<T>.Default;
            for (int i = 0; i < count; i++)
            {
                if (comparer.Equals(items[SlotOf(i)], item)) return true;
            }
            return false;
        }

        /// <summary>Copies the elements into <paramref name="array"/>, head first, starting at <paramref name="arrayIndex"/>.</summary>
        /// <param name="array">The destination array.</param>
        /// <param name="arrayIndex">The first index of <paramref name="array"/> written.</param>
        /// <exception cref="ArgumentNullException"><paramref name="array"/> is null.</exception>
        /// <exception cref="ArgumentOutOfRangeException"><paramref name="arrayIndex"/> is negative or past the end of <paramref name="array"/>.</exception>
        /// <exception cref="ArgumentException">The elements do not fit between <paramref name="arrayIndex"/> and the end of <paramref name="array"/>.</exception>
        public void CopyTo(T[] array, int arrayIndex)
        {
            if (array == null) throw new ArgumentNullException("array");
            if (arrayIndex < 0 || arrayIndex > array.Length) throw new ArgumentOutOfRangeException("arrayIndex");
            if (array.Length - arrayIndex < count)
            {
                throw new ArgumentException("Destination array is not long enough to copy all the items in the collection. Check array index and length.");
            }
            for (int i = 0; i < count; i++)
            {
                array[arrayIndex + i] = items[SlotOf(i)];
            }
        }

        /// <summary>Removes and returns the element at the head.</summary>
        /// <returns>The element that was at the head.</returns>
        /// <exception cref="InvalidOperationException">The queue is empty.</exception>
        public T Dequeue()
        {
            if (count == 0) throw new InvalidOperationException("Queue empty.");
            T item = items[head];
            items[head] = default(T);
            head = head + 1;
            if (head == items.Length) head = 0;
            count = count - 1;
            version = version + 1;
            return item;
        }

        /// <summary>Adds <paramref name="item"/> at the tail.</summary>
        /// <param name="item">The element to add; it can be null when <typeparamref name="T"/> allows it.</param>
        public void Enqueue(T item)
        {
            if (items == null || count == items.Length) Resize(GrownCapacity());
            items[SlotOf(count)] = item;
            count = count + 1;
            version = version + 1;
        }

        /// <summary>An enumerator over the elements, head first.</summary>
        /// <returns>The enumerator.</returns>
        public Enumerator GetEnumerator()
        {
            return new Enumerator(this);
        }

        /// <summary>Returns the element at the head without removing it.</summary>
        /// <returns>The element at the head.</returns>
        /// <exception cref="InvalidOperationException">The queue is empty.</exception>
        public T Peek()
        {
            if (count == 0) throw new InvalidOperationException("Queue empty.");
            return items[head];
        }

        /// <summary>A new array holding the elements, head first.</summary>
        /// <returns>The array.</returns>
        public T[] ToArray()
        {
            T[] result = new T[count];
            for (int i = 0; i < count; i++)
            {
                result[i] = items[SlotOf(i)];
            }
            return result;
        }

        /// <summary>Shrinks the storage to the number of elements, when fewer than 90 percent of it is in use.</summary>
        public void TrimExcess()
        {
            int capacity = items == null ? 0 : items.Length;
            if ((long)(count + 1) * 10 <= (long)capacity * 9)
            {
                Resize(count);
            }
        }

        // ---- ICollection and the enumerable interfaces ---------------------------------------------

        IEnumerator<T> IEnumerable<T>.GetEnumerator()
        {
            return new Enumerator(this);
        }

        IEnumerator IEnumerable.GetEnumerator()
        {
            return new Enumerator(this);
        }

        bool ICollection.IsSynchronized
        {
            get { return false; }
        }

        object ICollection.SyncRoot
        {
            get { return this; }
        }

        void ICollection.CopyTo(Array array, int index)
        {
            if (array == null) throw new ArgumentNullException("array");
            if (array.Rank != 1) throw new ArgumentException("Only single dimensional arrays are supported for the requested action.");
            if (array.GetLowerBound(0) != 0) throw new ArgumentException("The lower bound of target array must be zero.");
            if (index < 0 || index > array.Length) throw new ArgumentOutOfRangeException("index");
            if (array.Length - index < count)
            {
                throw new ArgumentException("Destination array is not long enough to copy all the items in the collection. Check array index and length.");
            }
            T[] typed = array as T[];
            if (typed != null)
            {
                try
                {
                    CopyTo(typed, index);
                }
                catch (ArrayTypeMismatchException)
                {
                    throw new InvalidCastException("At least one element in the source array could not be cast down to the destination array type.");
                }
                return;
            }
            try
            {
                object[] objects = array as object[];
                if (objects != null)
                {
                    for (int i = 0; i < count; i++)
                    {
                        objects[index + i] = items[SlotOf(i)];
                    }
                    return;
                }
                if (count == 0) return;
                int first = items.Length - head;
                if (first > count) first = count;
                Array.Copy(items, head, array, index, first);
                if (first < count) Array.Copy(items, 0, array, index + first, count - first);
            }
            catch (ArrayTypeMismatchException)
            {
                throw new ArgumentException("Target array type is not compatible with the type of items in the collection.");
            }
        }

        // ---- the buffer -------------------------------------------------------------------------------

        private int SlotOf(int position)
        {
            int slot = head + position;
            if (slot >= items.Length) slot = slot - items.Length;
            return slot;
        }

        private int GrownCapacity()
        {
            int length = items == null ? 0 : items.Length;
            int grown = length * 2;
            if (grown < length + 4) grown = length + 4;
            if (grown < 0) throw new OutOfMemoryException();
            return grown;
        }

        private void Resize(int capacity)
        {
            T[] resized = null;
            if (capacity > 0)
            {
                resized = new T[capacity];
                for (int i = 0; i < count; i++)
                {
                    resized[i] = items[SlotOf(i)];
                }
            }
            items = resized;
            head = 0;
            version = version + 1;
        }

        // ---- what the enumerator calls back into ------------------------------------------------------

        private T ItemAt(int position)
        {
            return items[SlotOf(position)];
        }

        private T DefaultItem()
        {
            return default(T);
        }

        private void CheckVersion(int expectedVersion)
        {
            if (expectedVersion != version)
            {
                throw new InvalidOperationException("Collection was modified; enumeration operation may not execute.");
            }
        }

        /// <summary>Enumerates the elements of a <see cref="Queue{T}"/>, head first.</summary>
        /// <remarks>
        /// Enqueue, Dequeue, Clear, and a TrimExcess that shrinks the storage invalidate the
        /// enumerator: its next MoveNext or Reset throws InvalidOperationException.
        /// </remarks>
        public struct Enumerator : IEnumerator<T>, IEnumerator, IDisposable
        {
            private Queue<T> queue;
            private int version;
            private int index;
            private T current;

            internal Enumerator(Queue<T> queue)
            {
                this.queue = queue;
                this.version = queue.version;
                this.index = -1;
                this.current = queue.DefaultItem();
            }

            /// <summary>Advances to the next element.</summary>
            /// <returns>True when positioned on an element; false past the last one.</returns>
            /// <exception cref="InvalidOperationException">The queue was changed after this enumerator was created.</exception>
            public bool MoveNext()
            {
                queue.CheckVersion(version);
                if (index == -2) return false;
                index = index + 1;
                if (index == queue.count)
                {
                    index = -2;
                    current = queue.DefaultItem();
                    return false;
                }
                current = queue.ItemAt(index);
                return true;
            }

            /// <summary>The element at the cursor.</summary>
            /// <exception cref="InvalidOperationException">The enumerator is before the first element or past the last.</exception>
            public T Current
            {
                get
                {
                    if (index < 0)
                    {
                        throw new InvalidOperationException(index == -1
                            ? "Enumeration has not started. Call MoveNext."
                            : "Enumeration already finished.");
                    }
                    return current;
                }
            }

            object IEnumerator.Current
            {
                get
                {
                    if (index < 0)
                    {
                        throw new InvalidOperationException(index == -1
                            ? "Enumeration has not started. Call MoveNext."
                            : "Enumeration already finished.");
                    }
                    return current;
                }
            }

            void IEnumerator.Reset()
            {
                queue.CheckVersion(version);
                index = -1;
                current = queue.DefaultItem();
            }

            /// <summary>Ends the enumeration: MoveNext answers false from now on.</summary>
            public void Dispose()
            {
                index = -2;
                if (queue != null) current = queue.DefaultItem();
            }
        }
    }
}
#endif
