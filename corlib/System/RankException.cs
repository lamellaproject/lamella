// Lamella managed corlib (from scratch). -- System.RankException
namespace System
{
    public class RankException : SystemException
    {
        public RankException() : base() { }
        public RankException(string message) : base(message) { }
        public RankException(string message, Exception innerException) : base(message, innerException) { }
    }
}
