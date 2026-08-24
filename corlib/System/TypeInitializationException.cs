// Lamella managed corlib (from scratch). -- System.TypeInitializationException
namespace System
{
    public sealed class TypeInitializationException : SystemException
    {
        private string _typeName;

        public TypeInitializationException(string fullTypeName, Exception innerException)
            : base("The type initializer for '" + fullTypeName + "' threw an exception.", innerException)
        {
            _typeName = fullTypeName;
        }

        [Lamella.Runtime.RuntimeProvided] private string RuntimeTypeName() { return null; }

        public string TypeName
        {
            get
            {
                string raised = RuntimeTypeName();
                if (raised != null) { return raised; }
                return _typeName == null ? "" : _typeName;
            }
        }
    }
}
