// Lamella managed corlib (from scratch). -- System.ArgumentNullException
namespace System
{
    public class ArgumentNullException : ArgumentException
    {
        public ArgumentNullException() : base() { }
        public ArgumentNullException(string message) : base(message) { }
        public ArgumentNullException(string message, Exception innerException) : base(message, innerException) { }
    }
}
