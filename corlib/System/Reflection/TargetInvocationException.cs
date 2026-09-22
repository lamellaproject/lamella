// Lamella managed corlib (from scratch). -- System.Reflection.TargetInvocationException
#if LAMELLA_SURFACE_REFLECTION
namespace System.Reflection
{
    public class TargetInvocationException : ApplicationException
    {
        public TargetInvocationException(Exception innerException)
            : base("Exception has been thrown by the target of an invocation.", innerException)
        {
        }

        public TargetInvocationException(string message, Exception innerException)
            : base(message, innerException)
        {
        }
    }
}
#endif
