// Lamella managed corlib (from scratch). -- System.MissingFieldException
namespace System
{
    public class MissingFieldException : MissingMemberException
    {
        public MissingFieldException() : base() { }
        public MissingFieldException(string message) : base(message) { }
        public MissingFieldException(string message, Exception innerException) : base(message, innerException) { }
    }
}
