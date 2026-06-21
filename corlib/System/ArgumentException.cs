// Lamella managed corlib (from scratch). -- System.ArgumentException
namespace System
{
    public class ArgumentException : SystemException
    {
        public ArgumentException() : base() { }
        public ArgumentException(string message) : base(message) { }
        public ArgumentException(string message, Exception innerException) : base(message, innerException) { }
    }
}
