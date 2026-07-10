// Lamella managed corlib (from scratch). -- System.IndexOutOfRangeException
namespace System
{
    public class IndexOutOfRangeException : SystemException
    {
        public IndexOutOfRangeException() : base() { }
        public IndexOutOfRangeException(string message) : base(message) { }
        public IndexOutOfRangeException(string message, Exception innerException) : base(message, innerException) { }
    }
}
