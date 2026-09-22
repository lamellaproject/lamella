// Lamella managed corlib (from scratch). -- System.Reflection.TargetParameterCountException
#if LAMELLA_SURFACE_REFLECTION
namespace System.Reflection
{
    public class TargetParameterCountException : ApplicationException
    {
        public TargetParameterCountException() : base() { }
        public TargetParameterCountException(string message) : base(message) { }
        public TargetParameterCountException(string message, Exception innerException) : base(message, innerException) { }
    }
}
#endif
