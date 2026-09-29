// Lamella managed corlib (from scratch). -- System.Collections.SortedListEnumerator
namespace System.Collections
{
    internal class SortedListEnumerator : IDictionaryEnumerator, IDisposable
    {
        internal const int Keys = 1;
        internal const int Values = 2;
        internal const int Entries = 3;

        private SortedList list;
        private int kind;
        private int index;
        private int end;
        private int version;
        private bool holding;
        private object key;
        private object value;

        public SortedListEnumerator(SortedList list, int kind)
        {
            this.list = list;
            this.kind = kind;
            this.end = list.Count;
            this.version = list.version;
        }

        public bool MoveNext()
        {
            CheckVersion();
            if (index < end)
            {
                key = list.GetKey(index);
                value = list.GetByIndex(index);
                index = index + 1;
                holding = true;
                return true;
            }
            key = null;
            value = null;
            holding = false;
            return false;
        }

        public DictionaryEntry Entry
        {
            get
            {
                CheckVersion();
                CheckHolding();
                return new DictionaryEntry(key, value);
            }
        }

        public object Key
        {
            get
            {
                CheckVersion();
                CheckHolding();
                return key;
            }
        }

        public object Value
        {
            get
            {
                CheckVersion();
                CheckHolding();
                return value;
            }
        }

        public object Current
        {
            get
            {
                CheckHolding();
                if (kind == Keys) return key;
                if (kind == Values) return value;
                return new DictionaryEntry(key, value);
            }
        }

        public void Reset()
        {
            CheckVersion();
            index = 0;
            holding = false;
            key = null;
            value = null;
        }

        private void CheckVersion()
        {
            if (version != list.version)
            {
                throw new InvalidOperationException("Collection was modified after the enumerator was instantiated.");
            }
        }

        private void CheckHolding()
        {
            if (!holding) throw new InvalidOperationException("Enumeration has either not started or has already finished.");
        }

        public void Dispose() { }
    }
}
