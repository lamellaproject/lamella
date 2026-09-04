// Lamella managed corlib (from scratch). -- System.Object
namespace System
{
    public class Object
    {
        public Object() { }
        public virtual bool Equals(object o) { return (object)this == o; }
        [Lamella.Runtime.RuntimeProvided] public virtual int GetHashCode() { return 0; }
        [Lamella.Runtime.RuntimeProvided] public virtual string ToString() { return null; }
        [Lamella.Runtime.RuntimeProvided] public Type GetType() { return null; }

        [Lamella.Runtime.RuntimeProvided] public static bool ReferenceEquals(object objA, object objB) { return false; }

        public static bool Equals(object objA, object objB)
        {
            if (objA == null) return objB == null;
            return objA.Equals(objB);
        }
    }
}
