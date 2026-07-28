// Lamella managed corlib (from scratch). -- System.Exception
namespace System
{
    public class Exception
    {
        private string _message;
        private Exception _innerException;

        public Exception() { }
        public Exception(string message) { _message = message; }
        public Exception(string message, Exception innerException)
        {
            _message = message;
            _innerException = innerException;
        }

        [Lamella.Runtime.RuntimeProvided] private string RuntimeMessage() { return null; }

        public virtual string Message
        {
            get
            {
                string raised = RuntimeMessage();
                if (raised != null) { return raised; }
                return _message;
            }
        }

        public Exception InnerException { get { return _innerException; } }
    }
}
