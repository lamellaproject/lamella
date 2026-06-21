// Lamella managed corlib (from scratch). -- System.Object
namespace System
{
    public class Object
    {
        public Object() { }
        public virtual bool Equals(object o) { return (object)this == o; }
        public virtual int GetHashCode() { return 0; }
        [Lamella.Runtime.RuntimeProvided] public virtual string ToString() { return null; }
    }
}
