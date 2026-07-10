// Lamella managed corlib (from scratch). -- System.OverflowException
namespace System
{
    public class OverflowException : ArithmeticException
    {
        public OverflowException() : base() { }
        public OverflowException(string message) : base(message) { }
        public OverflowException(string message, Exception innerException) : base(message, innerException) { }
    }
}
