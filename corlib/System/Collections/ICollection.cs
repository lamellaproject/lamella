// Lamella managed corlib (from scratch). -- System.Collections.ICollection
namespace System.Collections
{
    public interface ICollection : IEnumerable
    {
        int Count { get; }
        void CopyTo(System.Array array, int index);
    }
}
