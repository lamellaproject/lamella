// Lamella managed corlib (from scratch). -- System.InvalidOperationException
namespace System
{
    public class InvalidOperationException : SystemException
    {
        public InvalidOperationException() : base() { }
        public InvalidOperationException(string message) : base(message) { }
        public InvalidOperationException(string message, Exception innerException) : base(message, innerException) { }
    }
}
