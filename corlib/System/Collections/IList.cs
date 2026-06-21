// Lamella managed corlib (from scratch). -- System.Collections.IList
namespace System.Collections
{
    public interface IList : ICollection
    {
        object this[int index] { get; set; }
        int Add(object value);
        bool Contains(object value);
        int IndexOf(object value);
        void Insert(int index, object value);
        void Remove(object value);
        void RemoveAt(int index);
        void Clear();
        bool IsFixedSize { get; }
        bool IsReadOnly { get; }
    }
}
