// Lamella managed corlib (from scratch). -- System.TypeLoadException
namespace System
{
    public class TypeLoadException : SystemException
    {
        public TypeLoadException() : base() { }
        public TypeLoadException(string message) : base(message) { }
        public TypeLoadException(string message, Exception innerException) : base(message, innerException) { }
    }
}
