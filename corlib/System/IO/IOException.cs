// Lamella managed corlib (from scratch). -- System.IO.IOException
namespace System.IO
{
    public class IOException : SystemException
    {
        public IOException() : base() { }
        public IOException(string message) : base(message) { }
        public IOException(string message, Exception innerException) : base(message, innerException) { }
    }
}
