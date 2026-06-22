// Lamella managed corlib (from scratch). -- System.ObjectDisposedException
namespace System
{
    public class ObjectDisposedException : InvalidOperationException
    {
        private string _objectName;

        public ObjectDisposedException() : base() { }
        public ObjectDisposedException(string message) : base(message) { }
        public ObjectDisposedException(string message, Exception innerException) : base(message, innerException) { }

        public ObjectDisposedException(string objectName, string message) : base(message) { _objectName = objectName; }

        public string ObjectName { get { return _objectName; } }
    }
}
