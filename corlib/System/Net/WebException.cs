// Lamella managed corlib (from scratch). -- System.Net.WebException
#if LAMELLA_SURFACE_NET
namespace System.Net
{
    public class WebException : InvalidOperationException
    {
        private WebExceptionStatus _status;
        private WebResponse _response;

        public WebException() : base() { _status = WebExceptionStatus.UnknownError; }

        public WebException(string message) : base(message) { _status = WebExceptionStatus.UnknownError; }

        public WebException(string message, Exception innerException)
            : base(message, innerException) { _status = WebExceptionStatus.UnknownError; }

        public WebException(string message, WebExceptionStatus status)
            : base(message) { _status = status; }

        internal WebException(string message, WebExceptionStatus status, WebResponse response)
            : base(message) { _status = status; _response = response; }

        public WebExceptionStatus Status { get { return _status; } }

        public WebResponse Response { get { return _response; } }
    }
}
#endif
