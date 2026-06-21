// Lamella managed corlib (from scratch). -- System.Collections.IDictionary
namespace System.Collections
{
    public interface IDictionary : ICollection
    {
        object this[object key] { get; set; }
        void Add(object key, object value);
        bool Contains(object key);
        void Remove(object key);
        void Clear();
        bool IsFixedSize { get; }
        bool IsReadOnly { get; }
        ICollection Keys { get; }
        ICollection Values { get; }
    }
}
