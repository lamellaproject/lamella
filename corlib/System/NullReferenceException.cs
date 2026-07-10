// Lamella managed corlib (from scratch). -- System.NullReferenceException
namespace System
{
    public class NullReferenceException : SystemException
    {
        public NullReferenceException() : base() { }
        public NullReferenceException(string message) : base(message) { }
        public NullReferenceException(string message, Exception innerException) : base(message, innerException) { }
    }
}
