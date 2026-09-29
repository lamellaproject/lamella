// Lamella managed corlib (from scratch). -- System.ArgumentException
namespace System
{
    public class ArgumentException : SystemException
    {
        private string _paramName;

        public ArgumentException() : base("Value does not fall within the expected range.") { }
        public ArgumentException(string message) : base(message) { }
        public ArgumentException(string message, Exception innerException) : base(message, innerException) { }
        public ArgumentException(string message, string paramName) : base(message) { _paramName = paramName; }
        public ArgumentException(string message, string paramName, Exception innerException) : base(message, innerException) { _paramName = paramName; }

        public override string Message
        {
            get
            {
                string s = base.Message;
                if (RaisedByRuntime) return s;
                if (_paramName != null && _paramName.Length != 0)
                {
                    s = s + " (Parameter '" + _paramName + "')";
                }
                return s;
            }
        }

        public virtual string ParamName
        {
            get
            {
                if (RaisedByRuntime) return null;
                return _paramName;
            }
        }
    }
}
