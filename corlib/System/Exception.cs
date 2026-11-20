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

        internal bool RaisedByRuntime
        {
            get { return RuntimeMessage() != null; }
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

#if LAMELLA_SURFACE_NETFX_2_0
        /// <summary>Gets the runtime type of the current instance.</summary>
        /// <returns>The exact runtime type of the current instance.</returns>
        public new Type GetType()
        {
            return base.GetType();
        }
#endif

        public override string ToString()
        {
            string message = Message;
            Type type = ((object)this).GetType();
            string text = (object)type == null ? base.ToString() : type.ToString();
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
