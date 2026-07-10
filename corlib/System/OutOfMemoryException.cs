// Lamella managed corlib (from scratch). -- System.OutOfMemoryException
namespace System
{
    public class OutOfMemoryException : SystemException
    {
        public OutOfMemoryException() : base() { }
        public OutOfMemoryException(string message) : base(message) { }
        public OutOfMemoryException(string message, Exception innerException) : base(message, innerException) { }
    }
}
