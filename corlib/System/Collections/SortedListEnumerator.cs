// Lamella managed corlib (from scratch). -- System.Collections.SortedListEnumerator
namespace System.Collections
{
    internal class SortedListEnumerator : IDictionaryEnumerator, IDisposable
    {
        private object[] keys;
        private object[] values;
        private int count;
        private int index;

        public SortedListEnumerator(object[] keys, object[] values, int count)
        {
            this.keys = keys;
            this.values = values;
            this.count = count;
            this.index = -1;
        }

        public bool MoveNext()
        {
            index = index + 1;
            return index < count;
        }

        public DictionaryEntry Entry
        {
            get { return new DictionaryEntry(keys[index], values[index]); }
        }

        public object Key { get { return keys[index]; } }
        public object Value { get { return values[index]; } }

        public object Current { get { return new DictionaryEntry(keys[index], values[index]); } }

        public void Reset() { index = -1; }

        public void Dispose() { }
    }
}
