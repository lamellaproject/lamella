// Lamella managed corlib (from scratch). -- System.Reflection.EventInfo
namespace System.Reflection
{
    public class EventInfo : MemberInfo
    {
        private string _name;
        private MethodInfo _add;
        private MethodInfo _remove;

        internal EventInfo(string name, MethodInfo add, MethodInfo remove)
        {
            _name = name;
            _add = add;
            _remove = remove;
        }

        public override string Name { get { return _name; } }

        public Type EventHandlerType
        {
            get
            {
                if ((object)_add == null) return null;
                ParameterInfo[] parameters = _add.GetParameters();
                return parameters.Length > 0 ? parameters[0].ParameterType : null;
            }
        }

        public MethodInfo GetAddMethod() { return _add; }
        public MethodInfo GetRemoveMethod() { return _remove; }

        public void AddEventHandler(object target, Delegate handler)
        {
            if ((object)_add == null) throw new InvalidOperationException("The event has no add accessor.");
            _add.Invoke(target, new object[] { handler });
        }

        public void RemoveEventHandler(object target, Delegate handler)
        {
            if ((object)_remove == null) throw new InvalidOperationException("The event has no remove accessor.");
            _remove.Invoke(target, new object[] { handler });
        }

        public override object[] GetCustomAttributes(bool inherit) { return new object[0]; }
    }
}
