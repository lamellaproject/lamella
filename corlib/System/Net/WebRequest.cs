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

        public abstract string Method { get; set; }
        public abstract Uri RequestUri { get; }
        public abstract WebHeaderCollection Headers { get; set; }
        public abstract string ContentType { get; set; }
        public abstract long ContentLength { get; set; }

        public abstract Stream GetRequestStream();
        public abstract WebResponse GetResponse();
    }
}
#endif
