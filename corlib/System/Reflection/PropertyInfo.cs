// Lamella managed corlib (from scratch). -- System.Reflection.PropertyInfo
namespace System.Reflection
{
    public class PropertyInfo : MemberInfo
    {
        private string _name;
        private MethodInfo _getter;
        private MethodInfo _setter;
        private Type _propertyType;
        private Type _declaringType;

        internal PropertyInfo(string name, MethodInfo getter, MethodInfo setter, Type propertyType, Type declaringType)
        {
            _name = name;
            _getter = getter;
            _setter = setter;
            _propertyType = propertyType;
            _declaringType = declaringType;
        }

        public override string Name { get { return _name; } }

        public override object[] GetCustomAttributes(bool inherit)
        {
            return _declaringType.GetPropertyCustomAttributes(_name, inherit);
        }

        public Type PropertyType { get { return _propertyType; } }

        public bool CanRead { get { return (object)_getter != null; } }
        public bool CanWrite { get { return (object)_setter != null; } }

        public MethodInfo GetGetMethod() { return _getter; }
        public MethodInfo GetSetMethod() { return _setter; }

        public object GetValue(object obj, object[] index)
        {
            if ((object)_getter == null) throw new ArgumentException("Property does not have a get accessor.");
            return _getter.Invoke(obj, index);
        }

        public void SetValue(object obj, object value, object[] index)
        {
            if ((object)_setter == null) throw new ArgumentException("Property does not have a set accessor.");
            object[] args;
            if (index == null || index.Length == 0)
            {
                args = new object[] { value };
            }
            else
            {
                args = new object[index.Length + 1];
                for (int i = 0; i < index.Length; i++) args[i] = index[i];
                args[index.Length] = value;
            }
            _setter.Invoke(obj, args);
        }
    }
}
