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

        [Lamella.Runtime.RuntimeProvided] private Exception RuntimeInnerException() { return null; }

        public Exception InnerException
        {
            get
            {
                Exception raised = RuntimeInnerException();
                if (raised != null) { return raised; }
                return _innerException;
            }
        }

        public virtual Exception GetBaseException()
        {
            Exception current = this;
            while (current.InnerException != null)
            {
                current = current.InnerException;
            }
            return current;
        }

        public override string ToString()
        {
            string message = Message;
            string text = GetType().FullName;
            if (message != null && message.Length != 0)
            {
                text = text + ": " + message;
            }
            Exception inner = InnerException;
            if (inner != null)
            {
                text = text + Environment.NewLine + " ---> " + inner.ToString()
                    + Environment.NewLine + "   --- End of inner exception stack trace ---";
            }
            return text;
        }
    }
}
