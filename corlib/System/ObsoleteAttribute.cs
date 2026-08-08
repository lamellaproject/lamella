// Lamella managed corlib (from scratch). -- System.ObsoleteAttribute
namespace System
{
    [System.AttributeUsage(System.AttributeTargets.Class | System.AttributeTargets.Struct | System.AttributeTargets.Enum | System.AttributeTargets.Constructor | System.AttributeTargets.Method | System.AttributeTargets.Property | System.AttributeTargets.Field | System.AttributeTargets.Event | System.AttributeTargets.Interface | System.AttributeTargets.Delegate, Inherited = false)]
    public sealed class ObsoleteAttribute : System.Attribute
    {
        private readonly string _message;
        private readonly bool _error;

        public ObsoleteAttribute() { }

        public ObsoleteAttribute(string message)
        {
            _message = message;
        }

        public ObsoleteAttribute(string message, bool error)
        {
            _message = message;
            _error = error;
        }

        public string Message { get { return _message; } }

        public bool IsError { get { return _error; } }
    }
}
