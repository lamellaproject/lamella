// Lamella managed corlib (from scratch). -- System primitive value-type stubs
namespace System
{
    public struct Void { }
    public abstract class ValueType : Object
    {
        [Lamella.Runtime.RuntimeProvided] public override bool Equals(object obj) { return false; }
    }
}
