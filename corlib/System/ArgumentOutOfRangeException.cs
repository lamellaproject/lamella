// Lamella managed corlib (from scratch). -- System.ArgumentOutOfRangeException
namespace System
{
    public class ArgumentOutOfRangeException : ArgumentException
    {
        private object _actualValue;

        public ArgumentOutOfRangeException() : base("Specified argument was out of the range of valid values.") { }
        public ArgumentOutOfRangeException(string paramName) : base("Specified argument was out of the range of valid values.", paramName) { }
        public ArgumentOutOfRangeException(string paramName, string message) : base(message, paramName) { }
        public ArgumentOutOfRangeException(string message, Exception innerException) : base(message, innerException) { }
        public ArgumentOutOfRangeException(string paramName, object actualValue, string message) : base(message, paramName) { _actualValue = actualValue; }

        public virtual object ActualValue
        {
            get
            {
                if (RaisedByRuntime) return null;
                return _actualValue;
            }
        }

        public override string Message
        {
            get
            {
                string s = base.Message;
                if (RaisedByRuntime || _actualValue == null) return s;
                string valueMessage = "Actual value was " + _actualValue.ToString() + ".";
                if (s == null) return valueMessage;
                return s + Environment.NewLine + valueMessage;
            }
        }
    }
}
