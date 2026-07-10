// Lamella managed corlib (from scratch). -- System.ArithmeticException
namespace System
{
    public class ArithmeticException : SystemException
    {
        public ArithmeticException() : base() { }
        public ArithmeticException(string message) : base(message) { }
        public ArithmeticException(string message, Exception innerException) : base(message, innerException) { }
    }
}
