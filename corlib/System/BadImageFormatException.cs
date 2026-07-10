// Lamella managed corlib (from scratch). -- System.BadImageFormatException
namespace System
{
    public class BadImageFormatException : SystemException
    {
        public BadImageFormatException() : base() { }
        public BadImageFormatException(string message) : base(message) { }
        public BadImageFormatException(string message, Exception innerException) : base(message, innerException) { }
    }
}
