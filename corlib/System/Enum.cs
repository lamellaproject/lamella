// Lamella managed corlib (from scratch). -- System.Enum
namespace System
{
    public abstract class Enum : ValueType
    {
        [Lamella.Runtime.RuntimeProvided] public bool HasFlag(Enum flag) { return false; }
        [Lamella.Runtime.RuntimeProvided] public string ToString(string format) { return null; }
    }
}
