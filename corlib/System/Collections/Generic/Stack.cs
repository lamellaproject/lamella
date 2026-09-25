// Lamella managed corlib (from scratch). -- System.Collections.Generic.Stack<T>
#if LAMELLA_SURFACE_NETFX_2_0
namespace System.Collections.Generic
{

    /// <summary>A last-in, first-out collection of <typeparamref name="T"/>.</summary>
    /// <typeparam name="T">The type of the elements. An element can be null when the type allows it.</typeparam>
    public class Stack<T> : IEnumerable<T>, ICollection, IEnumerable
    {
        private T[] items;
        private int count;
        private int version;

        /// <summary>An empty stack.</summary>
        public Stack()
        {
        }

        /// <summary>An empty stack with room for <paramref name="capacity"/> elements before it grows.</summary>
        /// <param name="capacity">How many elements to make room for.</param>
        /// <exception cref="ArgumentOutOfRangeException"><paramref name="capacity"/> is negative.</exception>
        public Stack(int capacity)
        {
            if (capacity < 0) throw new ArgumentOutOfRangeException("capacity");
            if (capacity > 0) items = new T[capacity];
        }

        /// <summary>A stack holding the elements of <paramref name="collection"/>, pushed in enumeration order, so the last one is on top.</summary>
        /// <param name="collection">The elements to copy.</param>
        /// <exception cref="ArgumentNullException"><paramref name="collection"/> is null.</exception>
        public Stack(IEnumerable<T> collection)
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
                Push(item);
            }
        }

        /// <summary>How many elements the stack holds.</summary>
        public int Count
        {
            get { return count; }
        }

        /// <summary>Removes every element. The storage the stack has already made is kept.</summary>
        public void Clear()
        {
            for (int i = 0; i < count; i++)
            {
                items[i] = default(T);
            }
            count = 0;
            version = version + 1;
        }

        /// <summary>Whether the stack holds an element equal to <paramref name="item"/>, by the default equality comparer for <typeparamref name="T"/>.</summary>
        /// <param name="item">The element to look for; null matches a null element.</param>
        /// <returns>True when a matching element is present.</returns>
        public bool Contains(T item)
        {
            EqualityComparer<T> comparer = EqualityComparer<T>.Default;
            for (int i = count - 1; i >= 0; i--)
            {
                if (comparer.Equals(items[i], item)) return true;
            }
            return false;
        }

        /// <summary>Copies the elements into <paramref name="array"/>, top first, starting at <paramref name="arrayIndex"/>.</summary>
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
                array[arrayIndex + i] = items[count - 1 - i];
            }
        }

        /// <summary>An enumerator over the elements, top first.</summary>
        /// <returns>The enumerator.</returns>
        public Enumerator GetEnumerator()
        {
            return new Enumerator(this);
        }

        /// <summary>Returns the element on top without removing it.</summary>
        /// <returns>The element on top.</returns>
        /// <exception cref="InvalidOperationException">The stack is empty.</exception>
        public T Peek()
        {
            if (count == 0) throw new InvalidOperationException("Stack empty.");
            return items[count - 1];
        }

        /// <summary>Removes and returns the element on top.</summary>
        /// <returns>The element that was on top.</returns>
        /// <exception cref="InvalidOperationException">The stack is empty.</exception>
        public T Pop()
        {
            if (count == 0) throw new InvalidOperationException("Stack empty.");
            count = count - 1;
            T item = items[count];
            items[count] = default(T);
            version = version + 1;
            return item;
        }

        /// <summary>Puts <paramref name="item"/> on top.</summary>
        /// <param name="item">The element to add; it can be null when <typeparamref name="T"/> allows it.</param>
        public void Push(T item)
        {
            if (items == null || count == items.Length) Resize(GrownCapacity());
            items[count] = item;
            count = count + 1;
            version = version + 1;
        }

        /// <summary>A new array holding the elements, top first.</summary>
        /// <returns>The array.</returns>
        public T[] ToArray()
        {
            T[] result = new T[count];
            for (int i = 0; i < count; i++)
            {
                result[i] = items[count - 1 - i];
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

        void ICollection.CopyTo(Array array, int arrayIndex)
        {
            if (array == null) throw new ArgumentNullException("array");
            if (array.Rank != 1) throw new ArgumentException("Only single dimensional arrays are supported for the requested action.");
            if (array.GetLowerBound(0) != 0) throw new ArgumentException("The lower bound of target array must be zero.");
            if (arrayIndex < 0 || arrayIndex > array.Length) throw new ArgumentOutOfRangeException("arrayIndex");
            if (array.Length - arrayIndex < count)
            {
                throw new ArgumentException("Destination array is not long enough to copy all the items in the collection. Check array index and length.");
            }
            T[] typed = array as T[];
            if (typed != null)
            {
                try
                {
                    CopyTo(typed, arrayIndex);
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
                        objects[arrayIndex + i] = items[count - 1 - i];
                    }
                    return;
                }
                if (count == 0) return;
                Array.Copy(items, 0, array, arrayIndex, count);
                Array.Reverse(array, arrayIndex, count);
            }
            catch (ArrayTypeMismatchException)
            {
                throw new ArgumentException("Target array type is not compatible with the type of items in the collection.");
            }
        }

        // ---- the storage ------------------------------------------------------------------------------

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
                    resized[i] = items[i];
                }
            }
            items = resized;
        }

        // ---- what the enumerator calls back into ------------------------------------------------------

        private T ItemAt(int position)
        {
            return items[position];
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

        /// <summary>Enumerates the elements of a <see cref="Stack{T}"/>, top first.</summary>
        /// <remarks>
        /// Push, Pop and Clear invalidate the enumerator: its next MoveNext or Reset throws
        /// InvalidOperationException. TrimExcess does not.
        /// </remarks>
        public struct Enumerator : IEnumerator<T>, IEnumerator, IDisposable
        {
            private Stack<T> stack;
            private int version;
            private int index;
            private T current;

            internal Enumerator(Stack<T> stack)
            {
                this.stack = stack;
                this.version = stack.version;
                this.index = -2;
                this.current = stack.DefaultItem();
            }

            /// <summary>Advances to the next element, toward the bottom.</summary>
            /// <returns>True when positioned on an element; false past the last one.</returns>
            /// <exception cref="InvalidOperationException">The stack was changed after this enumerator was created.</exception>
            public bool MoveNext()
            {
                stack.CheckVersion(version);
                if (index == -1) return false;
                if (index == -2)
                {
                    index = stack.count - 1;
                }
                else
                {
                    index = index - 1;
                }
                if (index < 0)
                {
                    index = -1;
                    current = stack.DefaultItem();
                    return false;
                }
                current = stack.ItemAt(index);
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
                        throw new InvalidOperationException(index == -2
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
                        throw new InvalidOperationException(index == -2
                            ? "Enumeration has not started. Call MoveNext."
                            : "Enumeration already finished.");
                    }
                    return current;
                }
            }

            void IEnumerator.Reset()
            {
                stack.CheckVersion(version);
                index = -2;
                current = stack.DefaultItem();
            }

            /// <summary>Ends the enumeration: MoveNext answers false from now on.</summary>
            public void Dispose()
            {
                index = -1;
                if (stack != null) current = stack.DefaultItem();
            }
        }
    }
}
#endif
