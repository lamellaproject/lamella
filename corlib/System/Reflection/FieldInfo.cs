// Lamella managed corlib (from scratch). -- System.Reflection.FieldInfo
namespace System.Reflection
{
    public class FieldInfo : MemberInfo
    {
        protected FieldInfo() { }

        public System.Type FieldType
        {
            [Lamella.Runtime.RuntimeProvided] get { return null; }
        }

        [Lamella.Runtime.RuntimeProvided] public object GetValue(object obj) { return null; }

        [Lamella.Runtime.RuntimeProvided] public void SetValue(object obj, object value) { }
    }
}
