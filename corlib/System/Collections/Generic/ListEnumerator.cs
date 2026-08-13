// Lamella managed corlib (from scratch). -- System.Collections.Generic.ListEnumerator<T>
#if LAMELLA_SURFACE_NETFX_2_0
namespace System.Collections.Generic
{

    /// Walks a `List<T>`'s backing array by index.
    internal class ListEnumerator<T> : IEnumerator<T>, IEnumerator, IDisposable
    {
        private T[] items;
        private int count;
        private int index;

        public ListEnumerator(T[] items, int count)
        {
            this.items = items;
            this.count = count;
            this.index = -1;
        }

        /// Advances the cursor, returning false past the last element.
        public bool MoveNext()
        {
            this.index = this.index + 1;
            return this.index < this.count;
        }

        /// The element at the cursor, typed.
        public T Current
        {
            get { return this.items[this.index]; }
        }

        object IEnumerator.Current
        {
            get { return this.items[this.index]; }
        }

        /// Rewinds to before the first element.
        public void Reset()
        {
            this.index = -1;
        }

        /// Nothing to release; present so a `foreach` finally has a Dispose to bind.
        public void Dispose()
        {
        }
    }
}
#endif
