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

        [Lamella.Runtime.RuntimeProvided] internal object[] GetPropertyCustomAttributes(string name, bool inherit) { return null; }

        public System.Reflection.PropertyInfo GetProperty(string name)
        {
            System.Reflection.MethodInfo getter = GetMethod("get_" + name);
            System.Reflection.MethodInfo setter = GetMethod("set_" + name);
            if ((object)getter == null && (object)setter == null) return null;
            Type propertyType = (object)getter != null ? getter.ReturnType : null;
            return new System.Reflection.PropertyInfo(name, getter, setter, propertyType, this);
        }

        public System.Reflection.PropertyInfo[] GetProperties()
        {
            System.Reflection.MethodInfo[] methods = GetMethods(
                System.Reflection.BindingFlags.Public
                | System.Reflection.BindingFlags.Instance
                | System.Reflection.BindingFlags.Static);
            string[] names = new string[methods.Length];
            int count = 0;
            for (int i = 0; i < methods.Length; i++)
            {
                string methodName = methods[i].Name;
                string propertyName = null;
                if (methodName.Length > 4 && (methodName.StartsWith("get_") || methodName.StartsWith("set_")))
                {
                    propertyName = methodName.Substring(4);
                }
                if (propertyName == null) continue;
                bool seen = false;
                for (int j = 0; j < count; j++) { if (names[j] == propertyName) { seen = true; break; } }
                if (!seen) { names[count] = propertyName; count = count + 1; }
            }
            System.Reflection.PropertyInfo[] result = new System.Reflection.PropertyInfo[count];
            for (int i = 0; i < count; i++) result[i] = GetProperty(names[i]);
            return result;
        }

        public System.Reflection.EventInfo GetEvent(string name)
        {
            System.Reflection.MethodInfo add = GetMethod("add_" + name);
            System.Reflection.MethodInfo remove = GetMethod("remove_" + name);
            if ((object)add == null && (object)remove == null) return null;
            return new System.Reflection.EventInfo(name, add, remove);
        }

        public System.Reflection.EventInfo[] GetEvents()
        {
            System.Reflection.MethodInfo[] methods = GetMethods(
                System.Reflection.BindingFlags.Public
                | System.Reflection.BindingFlags.Instance
                | System.Reflection.BindingFlags.Static);
            string[] names = new string[methods.Length];
            int count = 0;
            for (int i = 0; i < methods.Length; i++)
            {
                string methodName = methods[i].Name;
                string eventName = null;
                if (methodName.Length > 4 && methodName.StartsWith("add_")) eventName = methodName.Substring(4);
                else if (methodName.Length > 7 && methodName.StartsWith("remove_")) eventName = methodName.Substring(7);
                if (eventName == null) continue;
                bool seen = false;
                for (int j = 0; j < count; j++) { if (names[j] == eventName) { seen = true; break; } }
                if (!seen) { names[count] = eventName; count = count + 1; }
            }
            System.Reflection.EventInfo[] result = new System.Reflection.EventInfo[count];
            for (int i = 0; i < count; i++) result[i] = GetEvent(names[i]);
            return result;
        }

        [Lamella.Runtime.RuntimeProvided] public static Type GetTypeFromHandle(RuntimeTypeHandle handle) { return null; }

        [Lamella.Runtime.RuntimeProvided] public static bool operator ==(Type left, Type right) { return false; }

        [Lamella.Runtime.RuntimeProvided] public static bool operator !=(Type left, Type right) { return false; }
    }
}
