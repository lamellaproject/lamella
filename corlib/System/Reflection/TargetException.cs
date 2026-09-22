// Lamella managed corlib (from scratch). -- System.Reflection.TargetException
#if LAMELLA_SURFACE_REFLECTION
namespace System.Reflection
{
    public class TargetException : ApplicationException
    {
        public TargetException() : base() { }
        public TargetException(string message) : base(message) { }
        public TargetException(string message, Exception innerException) : base(message, innerException) { }
    }
}
#endif
