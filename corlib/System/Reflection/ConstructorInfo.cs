// Lamella managed corlib (from scratch). -- System.Reflection.ConstructorInfo
namespace System.Reflection
{
    public class ConstructorInfo : MethodBase
    {
        protected ConstructorInfo() { }

        [Lamella.Runtime.RuntimeProvided] public object Invoke(object[] parameters) { return null; }
    }
}
