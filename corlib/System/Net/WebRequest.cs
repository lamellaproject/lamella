// Lamella managed corlib (from scratch). -- System.Net.WebRequest
#if LAMELLA_SURFACE_NET
using System.IO;

namespace System.Net
{
    public abstract class WebRequest
    {
        public static WebRequest Create(string requestUriString)
        {
            if ((object)requestUriString == null) throw new ArgumentNullException("requestUriString");
            return Create(new Uri(requestUriString));
        }

        public static WebRequest Create(Uri requestUri)
        {
            if ((object)requestUri == null) throw new ArgumentNullException("requestUri");
            string scheme = requestUri.Scheme;
            if (scheme == "http" || scheme == "https")
                return new HttpWebRequest(requestUri);
            throw new NotSupportedException("The request scheme '" + scheme + "' is not registered.");
        }

        private const string MethodNotImplemented = "This method is not implemented by this class.";
        private const string PropertyNotImplemented = "This property is not implemented by this class.";

        public virtual string Method
        {
            get { throw new NotImplementedException(PropertyNotImplemented); }
            set { throw new NotImplementedException(PropertyNotImplemented); }
        }

        public virtual Uri RequestUri { get { throw new NotImplementedException(PropertyNotImplemented); } }

        public virtual WebHeaderCollection Headers
        {
            get { throw new NotImplementedException(PropertyNotImplemented); }
            set { throw new NotImplementedException(PropertyNotImplemented); }
        }

        public virtual string ContentType
        {
            get { throw new NotImplementedException(PropertyNotImplemented); }
            set { throw new NotImplementedException(PropertyNotImplemented); }
        }

        public virtual long ContentLength
        {
            get { throw new NotImplementedException(PropertyNotImplemented); }
            set { throw new NotImplementedException(PropertyNotImplemented); }
        }

        public virtual Stream GetRequestStream() { throw new NotImplementedException(MethodNotImplemented); }
        public virtual WebResponse GetResponse() { throw new NotImplementedException(MethodNotImplemented); }
    }
}
#endif
