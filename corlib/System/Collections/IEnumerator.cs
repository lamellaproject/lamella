// Lamella managed corlib (from scratch). -- System.Collections.IEnumerator
namespace System.Collections
{
    public interface IEnumerator
    {
        bool MoveNext();
        object Current { get; }
        void Reset();
    }
}
