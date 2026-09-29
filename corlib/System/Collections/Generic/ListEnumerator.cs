// Lamella managed corlib (from scratch). -- System.Collections.Generic.ListEnumerator<T>
#if LAMELLA_SURFACE_NETFX_2_0
namespace System.Collections.Generic
{

    /// Walks a `List<T>`'s backing array by index.
    internal class ListEnumerator<T> : IEnumerator<T>, IEnumerator, IDisposable
    {
        private IEnumerationVersion owner;
        private T[] items;
        private int count;
        private int index;
        private int version;
        private T current;

        public ListEnumerator(IEnumerationVersion owner, T[] items, int count)
        {
            this.owner = owner;
            this.items = items;
            this.count = count;
            this.index = 0;
            this.version = owner.EnumerationVersion;
            this.current = default(T);
        }

        /// Advances the cursor, returning false past the last element.
        public bool MoveNext()
        {
            if (this.version == this.owner.EnumerationVersion && this.index < this.count)
            {
                this.current = this.items[this.index];
                this.index = this.index + 1;
                return true;
            }
            CheckVersion();
            this.index = this.count + 1;
            this.current = default(T);
            return false;
        }

        /// The element at the cursor, typed.
        public T Current
        {
            get { return this.current; }
        }

        object IEnumerator.Current
        {
            get
            {
                if (this.index == 0 || this.index == this.count + 1)
                {
                    throw new InvalidOperationException("Enumeration has either not started or has already finished.");
                }
                return this.current;
            }
        }

        /// Rewinds to before the first element.
        public void Reset()
        {
            CheckVersion();
            this.index = 0;
            this.current = default(T);
        }

        private void CheckVersion()
        {
            if (this.version != this.owner.EnumerationVersion)
            {
                throw new InvalidOperationException("Collection was modified; enumeration operation may not execute.");
            }
        }

        /// Nothing to release; present so a `foreach` finally has a Dispose to bind.
        public void Dispose()
        {
        }
    }
}
#endif
