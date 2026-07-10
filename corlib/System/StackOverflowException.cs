// Lamella managed corlib (from scratch). -- System.StackOverflowException
namespace System
{
    public class StackOverflowException : SystemException
    {
        public StackOverflowException() : base() { }
        public StackOverflowException(string message) : base(message) { }
        public StackOverflowException(string message, Exception innerException) : base(message, innerException) { }
    }
}
