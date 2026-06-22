// Lamella managed corlib (from scratch). -- System.DivideByZeroException
namespace System
{
    public class DivideByZeroException : SystemException
    {
        public DivideByZeroException() : base() { }
        public DivideByZeroException(string message) : base(message) { }
        public DivideByZeroException(string message, Exception innerException) : base(message, innerException) { }
    }
}
