// Lamella managed corlib (from scratch). -- System.Reflection.ConstructorInfo
#if LAMELLA_SURFACE_REFLECTION
namespace System.Reflection
{
    public class ConstructorInfo : MethodBase
    {
        protected ConstructorInfo() { }

        [Lamella.Runtime.RuntimeProvided] public object Invoke(object[] parameters) { return null; }
    }
}
#endif
