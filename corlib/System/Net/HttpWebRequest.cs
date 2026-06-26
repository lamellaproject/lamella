// Lamella managed corlib (from scratch). -- System.Net.HttpWebRequest
#if LAMELLA_SURFACE_NET
using System.IO;
using System.Net.Sockets;
using System.Text;
#if LAMELLA_SURFACE_NET_TLS
using System.Net.Security;
#endif

namespace System.Net
{
    public class HttpWebRequest : WebRequest
    {
        private Uri _uri;
        private string _method;
        private WebHeaderCollection _headers;
        private string _contentType;
        private long _contentLength;
        private HttpRequestStream _requestBody;
        private bool _allowAutoRedirect;
        private int _maxRedirects;

        internal HttpWebRequest(Uri uri)
        {
            _uri = uri;
            _method = "GET";
            _headers = new WebHeaderCollection();
            _contentLength = -1;
            _allowAutoRedirect = true;
            _maxRedirects = 50;
        }

        public override string Method
        {
            get { return _method; }
            set { _method = value; }
        }

        public override Uri RequestUri { get { return _uri; } }

        public Uri Address { get { return _uri; } }

        public override WebHeaderCollection Headers
        {
            get { return _headers; }
            set { _headers = value; }
        }

        public override string ContentType
        {
            get { return _contentType; }
            set { _contentType = value; }
        }

        public override long ContentLength
        {
            get { return _contentLength; }
            set { _contentLength = value; }
        }

        public string Accept
        {
            get { return _headers["Accept"]; }
            set { _headers.Set("Accept", value); }
        }

        public string UserAgent
        {
            get { return _headers["User-Agent"]; }
            set { _headers.Set("User-Agent", value); }
        }

        public bool AllowAutoRedirect
        {
            get { return _allowAutoRedirect; }
            set { _allowAutoRedirect = value; }
        }

        public int MaximumAutomaticRedirections
        {
            get { return _maxRedirects; }
            set
            {
                if (value <= 0) throw new ArgumentOutOfRangeException("value");
                _maxRedirects = value;
            }
        }

        public override Stream GetRequestStream()
        {
            if (_requestBody == null) _requestBody = new HttpRequestStream();
            return _requestBody;
        }

        public override WebResponse GetResponse()
        {
            Uri uri = _uri;
            string method = _method;
            byte[] body = _requestBody != null ? _requestBody.ToBytes() : null;
            string contentType = _contentType;
            int redirectCount = 0;

            while (true)
            {
                HttpWebResponse response = Execute(uri, method, body, contentType);
                int status = (int)response.StatusCode;

                if (_allowAutoRedirect && IsRedirect(status))
                {
                    string location = response.Headers["Location"];
                    if ((object)location != null && location.Length > 0)
                    {
                        if (redirectCount >= _maxRedirects)
                        {
                            response.Close();
                            throw new WebException("Too many automatic redirections were attempted.",
                                WebExceptionStatus.ProtocolError, response);
                        }
                        redirectCount++;
                        uri = ResolveLocation(uri, location);
                        if (status != 307 && status != 308 && method != "GET" && method != "HEAD")
                        {
                            method = "GET";
                            body = null;
                            contentType = null;
                        }
                        response.Close();
                        continue;
                    }
                }

                if (status >= 400)
                {
                    throw new WebException(
                        "The remote server returned an error: " + response.StatusDescription + ".",
                        WebExceptionStatus.ProtocolError, response);
                }
                return response;
            }
        }

        private HttpWebResponse Execute(Uri uri, string method, byte[] body, string contentType)
        {
            string scheme = uri.Scheme;
            IPAddress address = IPAddress.Parse(uri.Host);
            TcpClient client = new TcpClient();
            client.Connect(address, uri.Port);
            Stream net = client.GetStream();

            if (scheme == "https")
            {
#if LAMELLA_SURFACE_NET_TLS
                SslStream ssl = new SslStream(net, false, ServicePointManager.ServerCertificateValidationCallback);
                ssl.AuthenticateAsClient(uri.Host);
                net = ssl;
#else
                client.Close();
                System.Diagnostics.Debug.WriteLine(
                    "https requires the 'surface.net.tls' capability, which is off in this profile.");
                throw new NotSupportedException("The requested URI scheme 'https' is not supported.");
#endif
            }

            SendRequest(net, uri, method, body, contentType);
            HttpConnection conn = new HttpConnection(net, client);
            return HttpWebResponse.ReadFrom(conn, method, uri);
        }

        private static bool IsRedirect(int status)
        {
            return status == 301 || status == 302 || status == 303 || status == 307 || status == 308;
        }

        private static Uri ResolveLocation(Uri baseUri, string location)
        {
            if (location.StartsWith("http://") || location.StartsWith("https://"))
                return new Uri(location);
            string prefix = baseUri.Scheme + "://" + baseUri.Host + ":" + baseUri.Port.ToString();
            if (location[0] == '/')
                return new Uri(prefix + location);
            return new Uri(prefix + "/" + location);
        }

        private void SendRequest(Stream net, Uri uri, string method, byte[] body, string contentType)
        {
            StringBuilder sb = new StringBuilder();
            sb.Append(method);
            sb.Append(' ');
            sb.Append(uri.PathAndQuery);
            sb.Append(" HTTP/1.1\r\n");

            sb.Append("Host: ");
            sb.Append(uri.Host);
            if (!uri.IsDefaultPort)
            {
                sb.Append(':');
                sb.Append(uri.Port);
            }
            sb.Append("\r\n");

            if ((object)contentType != null)
            {
                sb.Append("Content-Type: ");
                sb.Append(contentType);
                sb.Append("\r\n");
            }
            if (body != null)
            {
                sb.Append("Content-Length: ");
                sb.Append(body.Length);
                sb.Append("\r\n");
            }
            for (int i = 0; i < _headers.Count; i++)
            {
                sb.Append(_headers.GetKey(i));
                sb.Append(": ");
                sb.Append(_headers.Get(i));
                sb.Append("\r\n");
            }
            sb.Append("Connection: close\r\n");
            sb.Append("\r\n");

            byte[] head = AsciiBytes(sb.ToString());
            net.Write(head, 0, head.Length);
            if (body != null && body.Length > 0) net.Write(body, 0, body.Length);
            net.Flush();
        }

        private static byte[] AsciiBytes(string s)
        {
            byte[] bytes = new byte[s.Length];
            for (int i = 0; i < s.Length; i++)
            {
                int c = s[i];
                bytes[i] = (byte)(c <= 0x7F ? c : (int)'?');
            }
            return bytes;
        }
    }
}
#endif
