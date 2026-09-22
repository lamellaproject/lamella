// Lamella managed corlib (from scratch). -- System.Reflection.MethodBase
#if LAMELLA_SURFACE_REFLECTION
namespace System.Reflection
{
    public class MethodBase : MemberInfo
    {
        protected MethodBase() { }

        public object Invoke(object obj, object[] parameters)
        {
            if (!IsStatic)
            {
                if ((object)obj == null)
                {
                    throw new TargetException("Non-static method requires a target.");
                }
                Type declaring = DeclaringType;
                if ((object)declaring != null && !declaring.IsInstanceOfType(obj))
                {
                    throw new TargetException(
                        "Object does not match target type.");
                }
            }
            int expected = GetParameterCount();
            int supplied = parameters == null ? 0 : parameters.Length;
            if (supplied != expected)
            {
                throw new TargetParameterCountException(
                    "Number of parameters specified does not match the expected number.");
            }
            for (int i = 0; i < expected; i++)
            {
                object argument = parameters[i];
                if (argument == null)
                {
                    continue;
                }
                Type parameterType = GetParameterType(i);
                if ((object)parameterType != null && !parameterType.IsInstanceOfType(argument))
                {
                    throw new ArgumentException(
                        "Object of type '" + argument.GetType().FullName
                        + "' cannot be converted to type '" + parameterType.FullName + "'.");
                }
            }
            return InvokeCore(obj, parameters);
        }

        [Lamella.Runtime.RuntimeProvided] private object InvokeCore(object obj, object[] parameters) { return null; }

        public ParameterInfo[] GetParameters()
        {
            int count = GetParameterCount();
            ParameterInfo[] result = new ParameterInfo[count];
            for (int i = 0; i < count; i++)
            {
                result[i] = new ParameterInfo(this, i, GetParameterType(i), GetParameterName(i));
            }
            return result;
        }

        [Lamella.Runtime.RuntimeProvided] internal int GetParameterCount() { return 0; }
        [Lamella.Runtime.RuntimeProvided] internal Type GetParameterType(int index) { return null; }
        [Lamella.Runtime.RuntimeProvided] internal string GetParameterName(int index) { return null; }

        [Lamella.Runtime.RuntimeProvided] internal object[] GetParameterCustomAttributes(int position, bool inherit) { return null; }

        public bool IsPublic { [Lamella.Runtime.RuntimeProvided] get { return false; } }

        public bool IsStatic { [Lamella.Runtime.RuntimeProvided] get { return false; } }

        public bool IsFinal { [Lamella.Runtime.RuntimeProvided] get { return false; } }

        public bool IsVirtual { [Lamella.Runtime.RuntimeProvided] get { return false; } }

        public bool IsAbstract { [Lamella.Runtime.RuntimeProvided] get { return false; } }
    }
}
#endif
