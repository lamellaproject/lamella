// Lamella managed corlib (from scratch). -- System.FormatException
namespace System
{
    public class FormatException : SystemException
    {
        public FormatException() : base() { }
        public FormatException(string message) : base(message) { }
        public FormatException(string message, Exception innerException) : base(message, innerException) { }
    }
}
