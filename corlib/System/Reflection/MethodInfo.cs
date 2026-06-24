// Lamella managed corlib (from scratch). -- System.Reflection.MethodInfo
namespace System.Reflection
{
    public class MethodInfo : MethodBase
    {
        protected MethodInfo() { }

        public System.Type ReturnType
        {
            [Lamella.Runtime.RuntimeProvided] get { return null; }
        }
    }
}
