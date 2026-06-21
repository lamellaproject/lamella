// Lamella managed corlib (from scratch). -- System.Collections.IDictionaryEnumerator
namespace System.Collections
{
    public interface IDictionaryEnumerator : IEnumerator
    {
        DictionaryEntry Entry { get; }
        object Key { get; }
        object Value { get; }
    }
}
