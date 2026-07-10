// Lamella managed corlib (from scratch). -- System.TypeInitializationException
namespace System
{
    public class TypeInitializationException : SystemException
    {
        private string _typeName;

        public TypeInitializationException(string fullTypeName, Exception innerException)
            : base("The type initializer for '" + fullTypeName + "' threw an exception.", innerException)
        {
            _typeName = fullTypeName;
        }

        public string TypeName { get { return _typeName == null ? "" : _typeName; } }
    }
}
