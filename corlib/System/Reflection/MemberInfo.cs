// Lamella managed corlib (from scratch). -- System.Reflection.MemberInfo
namespace System.Reflection
{
    public class MemberInfo
    {
        protected MemberInfo() { }

        public virtual string Name
        {
            [Lamella.Runtime.RuntimeProvided] get { return null; }
        }

        [Lamella.Runtime.RuntimeProvided] public virtual object[] GetCustomAttributes(bool inherit) { return null; }
    }
}
