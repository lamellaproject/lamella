// Lamella managed corlib (from scratch). -- System.TimeoutException
namespace System
{
    public class TimeoutException : SystemException
    {
        public TimeoutException() : base("The operation has timed out.") { }
        public TimeoutException(string message) : base(message) { }
        public TimeoutException(string message, Exception innerException) : base(message, innerException) { }
    }
}
