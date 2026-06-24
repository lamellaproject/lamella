// Lamella managed corlib (from scratch). -- System.Type
namespace System
{
    public class Type : System.Reflection.MemberInfo
    {
        public string FullName
        {
            [Lamella.Runtime.RuntimeProvided] get { return null; }
        }

        public string Namespace
        {
            [Lamella.Runtime.RuntimeProvided] get { return null; }
        }

        public System.Reflection.Assembly Assembly
        {
            [Lamella.Runtime.RuntimeProvided] get { return null; }
        }

        public bool IsEnum
        {
            [Lamella.Runtime.RuntimeProvided] get { return false; }
        }

        public bool IsValueType
        {
            [Lamella.Runtime.RuntimeProvided] get { return false; }
        }

        public bool IsClass
        {
            [Lamella.Runtime.RuntimeProvided] get { return false; }
        }

        public bool IsInterface
        {
            [Lamella.Runtime.RuntimeProvided] get { return false; }
        }

        public bool IsAbstract
        {
            [Lamella.Runtime.RuntimeProvided] get { return false; }
        }

        public bool IsPublic
        {
            [Lamella.Runtime.RuntimeProvided] get { return false; }
        }

        public bool IsNotPublic
        {
            [Lamella.Runtime.RuntimeProvided] get { return false; }
        }

        public bool IsArray
        {
            [Lamella.Runtime.RuntimeProvided] get { return false; }
        }

        public Type BaseType
        {
            [Lamella.Runtime.RuntimeProvided] get { return null; }
        }

        [Lamella.Runtime.RuntimeProvided] public System.Reflection.FieldInfo GetField(string name) { return null; }

        [Lamella.Runtime.RuntimeProvided] public System.Reflection.MethodInfo GetMethod(string name) { return null; }

        [Lamella.Runtime.RuntimeProvided] public System.Reflection.FieldInfo[] GetFields(System.Reflection.BindingFlags bindingAttr) { return null; }

        [Lamella.Runtime.RuntimeProvided] public System.Reflection.MethodInfo[] GetMethods(System.Reflection.BindingFlags bindingAttr) { return null; }

        [Lamella.Runtime.RuntimeProvided] public System.Reflection.ConstructorInfo GetConstructor(Type[] types) { return null; }

        [Lamella.Runtime.RuntimeProvided] public static Type GetTypeFromHandle(RuntimeTypeHandle handle) { return null; }

        [Lamella.Runtime.RuntimeProvided] public static bool operator ==(Type left, Type right) { return false; }

        [Lamella.Runtime.RuntimeProvided] public static bool operator !=(Type left, Type right) { return false; }
    }
}
