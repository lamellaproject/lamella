// Lamella managed corlib (from scratch). -- System.Reflection.MethodBase
namespace System.Reflection
{
    public class MethodBase : MemberInfo
    {
        protected MethodBase() { }

        [Lamella.Runtime.RuntimeProvided] public object Invoke(object obj, object[] parameters) { return null; }

        public bool IsPublic { [Lamella.Runtime.RuntimeProvided] get { return false; } }

        public bool IsStatic { [Lamella.Runtime.RuntimeProvided] get { return false; } }

        public bool IsFinal { [Lamella.Runtime.RuntimeProvided] get { return false; } }

        public bool IsVirtual { [Lamella.Runtime.RuntimeProvided] get { return false; } }

        public bool IsAbstract { [Lamella.Runtime.RuntimeProvided] get { return false; } }
    }
}
