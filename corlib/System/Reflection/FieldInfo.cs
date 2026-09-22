// Lamella managed corlib (from scratch). -- System.Reflection.FieldInfo
#if LAMELLA_SURFACE_REFLECTION
namespace System.Reflection
{
    public class FieldInfo : MemberInfo
    {
        protected FieldInfo() { }

        public System.Type FieldType
        {
            [Lamella.Runtime.RuntimeProvided] get { return null; }
        }

        public object GetValue(object obj)
        {
            if (!IsStatic && (object)obj == null)
            {
                throw new TargetException("Non-static field requires a target.");
            }
            return GetValueCore(obj);
        }

        public void SetValue(object obj, object value)
        {
            if (!IsStatic && (object)obj == null)
            {
                throw new TargetException("Non-static field requires a target.");
            }
            SetValueCore(obj, value);
        }

        [Lamella.Runtime.RuntimeProvided] private object GetValueCore(object obj) { return null; }

        [Lamella.Runtime.RuntimeProvided] private void SetValueCore(object obj, object value) { }

        public bool IsLiteral
        {
            [Lamella.Runtime.RuntimeProvided] get { return false; }
        }

        public bool IsStatic
        {
            [Lamella.Runtime.RuntimeProvided] get { return false; }
        }

        [Lamella.Runtime.RuntimeProvided] public object GetRawConstantValue() { return null; }
    }
}
#endif
