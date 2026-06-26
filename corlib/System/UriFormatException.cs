// Lamella managed corlib (from scratch). -- System.UriFormatException
namespace System
{
    public class UriFormatException : FormatException
    {
        public UriFormatException() : base() { }
        public UriFormatException(string textString) : base(textString) { }
        public UriFormatException(string textString, Exception e) : base(textString, e) { }
    }
}
