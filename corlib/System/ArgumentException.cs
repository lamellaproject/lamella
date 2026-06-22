// Lamella managed corlib (from scratch). -- System.ArgumentException
namespace System
{
    public class ArgumentException : SystemException
    {
        private string _paramName;

        public ArgumentException() : base() { }
        public ArgumentException(string message) : base(message) { }
        public ArgumentException(string message, Exception innerException) : base(message, innerException) { }
        public ArgumentException(string message, string paramName) : base(message) { _paramName = paramName; }
        public ArgumentException(string message, string paramName, Exception innerException) : base(message, innerException) { _paramName = paramName; }

        public virtual string ParamName { get { return _paramName; } }
    }
}
