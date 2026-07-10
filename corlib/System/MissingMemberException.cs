// Lamella managed corlib (from scratch). -- System.MissingMemberException
namespace System
{
    public class MissingMemberException : MemberAccessException
    {
        public MissingMemberException() : base() { }
        public MissingMemberException(string message) : base(message) { }
        public MissingMemberException(string message, Exception innerException) : base(message, innerException) { }
    }
}
