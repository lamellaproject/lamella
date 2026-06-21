// Lamella managed corlib (from scratch). -- System.NotSupportedException
namespace System
{
    public class NotSupportedException : SystemException
    {
        public NotSupportedException() : base() { }
        public NotSupportedException(string message) : base(message) { }
        public NotSupportedException(string message, Exception innerException) : base(message, innerException) { }
    }
}
