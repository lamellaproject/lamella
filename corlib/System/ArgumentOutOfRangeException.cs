// Lamella managed corlib (from scratch). -- System.ArgumentOutOfRangeException
namespace System
{
    public class ArgumentOutOfRangeException : ArgumentException
    {
        public ArgumentOutOfRangeException() : base() { }
        public ArgumentOutOfRangeException(string message) : base(message) { }
        public ArgumentOutOfRangeException(string message, Exception innerException) : base(message, innerException) { }
    }
}
