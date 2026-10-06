// Lamella managed corlib (from scratch). -- System.Collections.CollectionBase
namespace System.Collections
{

    /// <summary>
    /// The base of a strongly typed collection: an <see cref="ArrayList"/> the derived class reaches as
    /// <see cref="InnerList"/>, and an <see cref="IList"/> view, <see cref="List"/>, whose changes
    /// call the <c>On</c> methods so the derived class can validate and observe them.
    /// </summary>
    public abstract class CollectionBase : IList
    {
        private const string IndexOutOfRange =
            "Index was out of range. Must be non-negative and less than the size of the collection.";

        private ArrayList _list;

        /// <summary>Initializes an empty collection with the default capacity.</summary>
        protected CollectionBase()
        {
            _list = new ArrayList();
        }

#if LAMELLA_SURFACE_NETFX_2_0
        /// <summary>Initializes an empty collection that can hold <paramref name="capacity"/> elements before it grows.</summary>
        /// <exception cref="ArgumentOutOfRangeException"><paramref name="capacity"/> is less than zero.</exception>
        protected CollectionBase(int capacity)
        {
            _list = new ArrayList(capacity);
        }

        /// <summary>The number of elements the collection can hold before it grows.</summary>
        /// <exception cref="ArgumentOutOfRangeException">The value is less than <see cref="Count"/>.</exception>
        public int Capacity
        {
            get { return InnerList.Capacity; }
            set { InnerList.Capacity = value; }
        }
#endif

        /// <summary>The list that holds the elements. Changing it directly calls none of the <c>On</c> methods.</summary>
        protected ArrayList InnerList
        {
            get
            {
                if ((object)_list == null) _list = new ArrayList();
                return _list;
            }
        }

        /// <summary>The collection as an <see cref="IList"/>: its changes call the <c>On</c> methods.</summary>
        protected IList List
        {
            get { return this; }
        }

        /// <summary>The number of elements.</summary>
        public int Count
        {
            get { return (object)_list == null ? 0 : _list.Count; }
        }

        /// <summary>Removes every element, calling <see cref="OnClear"/> before and <see cref="OnClearComplete"/> after.</summary>
        public void Clear()
        {
            OnClear();
            InnerList.Clear();
            OnClearComplete();
        }

        /// <summary>Removes the element at <paramref name="index"/>.</summary>
        /// <exception cref="ArgumentOutOfRangeException"><paramref name="index"/> is less than zero, or not less than <see cref="Count"/>.</exception>
        public void RemoveAt(int index)
        {
            if (index < 0 || index >= Count) throw new ArgumentOutOfRangeException("index", IndexOutOfRange);
            object removed = InnerList[index];
            OnValidate(removed);
            OnRemove(index, removed);
            InnerList.RemoveAt(index);
            try
            {
                OnRemoveComplete(index, removed);
            }
            catch
            {
                InnerList.Insert(index, removed);
                throw;
            }
        }

        /// <summary>An enumerator over the elements.</summary>
        public IEnumerator GetEnumerator()
        {
            return InnerList.GetEnumerator();
        }

        bool IList.IsReadOnly
        {
            get { return InnerList.IsReadOnly; }
        }

        bool IList.IsFixedSize
        {
            get { return InnerList.IsFixedSize; }
        }

        bool ICollection.IsSynchronized
        {
            get { return InnerList.IsSynchronized; }
        }

        object ICollection.SyncRoot
        {
            get { return InnerList.SyncRoot; }
        }

        void ICollection.CopyTo(Array array, int index)
        {
            InnerList.CopyTo(array, index);
        }

        object IList.this[int index]
        {
            get { return GetItem(index); }
            set { SetItem(index, value); }
        }

        bool IList.Contains(object value)
        {
            return InnerList.Contains(value);
        }

        int IList.Add(object value)
        {
            return AddItem(value);
        }

        void IList.Remove(object value)
        {
            RemoveItem(value);
        }

        int IList.IndexOf(object value)
        {
            return InnerList.IndexOf(value);
        }

        void IList.Insert(int index, object value)
        {
            InsertItem(index, value);
        }

        internal object GetItem(int index)
        {
            if (index < 0 || index >= Count) throw new ArgumentOutOfRangeException("index", IndexOutOfRange);
            return InnerList[index];
        }

        internal void SetItem(int index, object value)
        {
            if (index < 0 || index >= Count) throw new ArgumentOutOfRangeException("index", IndexOutOfRange);
            OnValidate(value);
            object previous = InnerList[index];
            OnSet(index, previous, value);
            InnerList[index] = value;
            try
            {
                OnSetComplete(index, previous, value);
            }
            catch
            {
                InnerList[index] = previous;
                throw;
            }
        }

        internal int AddItem(object value)
        {
            OnValidate(value);
            OnInsert(InnerList.Count, value);
            int index = InnerList.Add(value);
            try
            {
                OnInsertComplete(index, value);
            }
            catch
            {
                InnerList.RemoveAt(index);
                throw;
            }
            return index;
        }

        internal void RemoveItem(object value)
        {
            OnValidate(value);
            int index = InnerList.IndexOf(value);
            if (index < 0) throw new ArgumentException("Cannot remove the specified item because it was not found in the specified Collection.");
            OnRemove(index, value);
            InnerList.RemoveAt(index);
            try
            {
                OnRemoveComplete(index, value);
            }
            catch
            {
                InnerList.Insert(index, value);
                throw;
            }
        }

        internal void InsertItem(int index, object value)
        {
            if (index < 0 || index > Count) throw new ArgumentOutOfRangeException("index", IndexOutOfRange);
            OnValidate(value);
            OnInsert(index, value);
            InnerList.Insert(index, value);
            try
            {
                OnInsertComplete(index, value);
            }
            catch
            {
                InnerList.RemoveAt(index);
                throw;
            }
        }

        /// <summary>Runs before an element is replaced through <see cref="List"/>.</summary>
        protected virtual void OnSet(int index, object oldValue, object newValue) { }

        /// <summary>Runs before an element is inserted through <see cref="List"/>.</summary>
        protected virtual void OnInsert(int index, object value) { }

        /// <summary>Runs before the collection is cleared.</summary>
        protected virtual void OnClear() { }

        /// <summary>Runs before an element is removed.</summary>
        protected virtual void OnRemove(int index, object value) { }

        /// <summary>Checks an element before it is added, inserted, set or removed. The base refuses only <see langword="null"/>.</summary>
        /// <exception cref="ArgumentNullException"><paramref name="value"/> is <see langword="null"/>.</exception>
        protected virtual void OnValidate(object value)
        {
            if (value == null) throw new ArgumentNullException("value");
        }

        /// <summary>Runs after an element is replaced through <see cref="List"/>.</summary>
        protected virtual void OnSetComplete(int index, object oldValue, object newValue) { }

        /// <summary>Runs after an element is inserted through <see cref="List"/>.</summary>
        protected virtual void OnInsertComplete(int index, object value) { }

        /// <summary>Runs after the collection is cleared.</summary>
        protected virtual void OnClearComplete() { }

        /// <summary>Runs after an element is removed.</summary>
        protected virtual void OnRemoveComplete(int index, object value) { }
    }
}
