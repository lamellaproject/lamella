// Lamella managed corlib (from scratch). -- System.MissingMethodException
namespace System
{
    public class MissingMethodException : MissingMemberException
    {
        public MissingMethodException() : base() { }
        public MissingMethodException(string message) : base(message) { }
        public MissingMethodException(string message, Exception innerException) : base(message, innerException) { }
    }
}
