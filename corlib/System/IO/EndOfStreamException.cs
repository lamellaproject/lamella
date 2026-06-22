// Lamella managed corlib (from scratch). -- System.IO.EndOfStreamException
namespace System.IO
{
    public class EndOfStreamException : IOException
    {
        public EndOfStreamException() : base() { }
        public EndOfStreamException(string message) : base(message) { }
        public EndOfStreamException(string message, Exception innerException) : base(message, innerException) { }
    }
}
