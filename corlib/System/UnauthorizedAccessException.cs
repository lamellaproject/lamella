// Lamella managed corlib (from scratch). -- System.UnauthorizedAccessException
namespace System
{
    public class UnauthorizedAccessException : SystemException
    {
        public UnauthorizedAccessException() : base() { }
        public UnauthorizedAccessException(string message) : base(message) { }
        public UnauthorizedAccessException(string message, Exception innerException) : base(message, innerException) { }
    }
}
