// Lamella managed corlib (from scratch). -- System.Collections.HashtableEnumerator
namespace System.Collections
{
    internal class HashtableEnumerator : IDictionaryEnumerator, IDisposable
    {
        internal const int Keys = 1;
        internal const int Values = 2;
        internal const int Entries = 3;

        private Hashtable table;
        private int kind;
        private int next;
        private int version;
        private bool holding;
        private object key;
        private object value;

        public HashtableEnumerator(Hashtable table, int kind)
        {
            this.table = table;
            this.kind = kind;
            this.version = table.version;
        }

        public bool MoveNext()
        {
            if (version != table.version)
            {
                throw new InvalidOperationException("Collection was modified; enumeration operation may not execute.");
            }
            if (next >= 0)
            {
                int slot = table.NextEntry(next);
                if (slot >= 0)
                {
                    key = table.KeyAt(slot);
                    value = table.ValueAt(slot);
                    holding = true;
                    next = slot + 1;
                    return true;
                }
                next = -1;
            }
            holding = false;
            return false;
        }

        public DictionaryEntry Entry
        {
            get
            {
                if (!holding) throw new InvalidOperationException("Enumeration has either not started or has already finished.");
                return new DictionaryEntry(key, value);
            }
        }

        public object Key
        {
            get
            {
                if (!holding) throw new InvalidOperationException("Enumeration has not started. Call MoveNext.");
                return key;
            }
        }

        public object Value
        {
            get
            {
                if (!holding) throw new InvalidOperationException("Enumeration has either not started or has already finished.");
                return value;
            }
        }

        public object Current
        {
            get
            {
                if (!holding) throw new InvalidOperationException("Enumeration has either not started or has already finished.");
                if (kind == Keys) return key;
                if (kind == Values) return value;
                return new DictionaryEntry(key, value);
            }
        }

        public void Reset()
        {
            if (version != table.version)
            {
                throw new InvalidOperationException("Collection was modified; enumeration operation may not execute.");
            }
            next = 0;
            holding = false;
            key = null;
            value = null;
        }

        public void Dispose() { }
    }
}
