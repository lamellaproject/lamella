// Lamella managed corlib (from scratch). -- System.SZGenericArrayEnumerator<T>
#if LAMELLA_SURFACE_NETFX_2_0
namespace System
{
    internal sealed class SZGenericArrayEnumerator<T> : System.Collections.Generic.IEnumerator<T>, System.Collections.IEnumerator, IDisposable
    {
        private static SZGenericArrayEnumerator<T> empty;

        private T[] array;
        private int index;

        private SZGenericArrayEnumerator(T[] array)
        {
            this.array = array;
            this.index = -1;
        }

        internal static System.Collections.Generic.IEnumerator<T> Of(T[] array)
        {
            if (array.Rank != 1)
            {
                throw new InvalidCastException();
            }
            if (array.Length == 0)
            {
                if (empty == null)
                {
                    empty = new SZGenericArrayEnumerator<T>(array);
                }
                return empty;
            }
            return new SZGenericArrayEnumerator<T>(array);
        }

        public bool MoveNext()
        {
            int next = index + 1;
            if (next < array.Length)
            {
                index = next;
                return true;
            }
            index = array.Length;
            return false;
        }

        public T Current
        {
            get
            {
                if (index < 0)
                {
                    throw new InvalidOperationException("Enumeration has not started. Call MoveNext.");
                }
                if (index >= array.Length)
                {
                    throw new InvalidOperationException("Enumeration already finished.");
                }
                return array[index];
            }
        }

        object System.Collections.IEnumerator.Current
        {
            get { return Current; }
        }

        void System.Collections.IEnumerator.Reset()
        {
            index = -1;
        }

        public void Dispose()
        {
        }
    }
}
#endif
