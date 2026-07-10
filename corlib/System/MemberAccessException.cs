// Lamella managed corlib (from scratch). -- System.MemberAccessException
namespace System
{
    public class MemberAccessException : SystemException
    {
        public MemberAccessException() : base() { }
        public MemberAccessException(string message) : base(message) { }
        public MemberAccessException(string message, Exception innerException) : base(message, innerException) { }
    }
}
