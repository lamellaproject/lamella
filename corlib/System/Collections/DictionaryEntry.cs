// Lamella managed corlib (from scratch). -- System.Collections.DictionaryEntry
namespace System.Collections
{
    public struct DictionaryEntry
    {
        private object _key;
        private object _value;

        public DictionaryEntry(object key, object value)
        {
            _key = key;
            _value = value;
        }

        public object Key
        {
            get { return _key; }
            set { _key = value; }
        }

        public object Value
        {
            get { return _value; }
            set { _value = value; }
        }
    }
}
