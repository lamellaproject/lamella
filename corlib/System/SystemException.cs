// Lamella managed corlib (from scratch). -- System.SystemException
namespace System
{
    public class SystemException : Exception
    {
        public SystemException() : base() { }
        public SystemException(string message) : base(message) { }
        public SystemException(string message, Exception innerException) : base(message, innerException) { }
    }
}
