// Lamella managed corlib (from scratch). -- System.Reflection.MethodBase
namespace System.Reflection
{
    public class MethodBase : MemberInfo
    {
        protected MethodBase() { }

        [Lamella.Runtime.RuntimeProvided] public object Invoke(object obj, object[] parameters) { return null; }

        public ParameterInfo[] GetParameters()
        {
            int count = GetParameterCount();
            ParameterInfo[] result = new ParameterInfo[count];
            for (int i = 0; i < count; i++)
            {
                result[i] = new ParameterInfo(i, GetParameterType(i), GetParameterName(i));
            }
            return result;
        }

        [Lamella.Runtime.RuntimeProvided] internal int GetParameterCount() { return 0; }
        [Lamella.Runtime.RuntimeProvided] internal Type GetParameterType(int index) { return null; }
        [Lamella.Runtime.RuntimeProvided] internal string GetParameterName(int index) { return null; }

        public bool IsPublic { [Lamella.Runtime.RuntimeProvided] get { return false; } }

        public bool IsStatic { [Lamella.Runtime.RuntimeProvided] get { return false; } }

        public bool IsFinal { [Lamella.Runtime.RuntimeProvided] get { return false; } }

        public bool IsVirtual { [Lamella.Runtime.RuntimeProvided] get { return false; } }

        public bool IsAbstract { [Lamella.Runtime.RuntimeProvided] get { return false; } }
    }
}
