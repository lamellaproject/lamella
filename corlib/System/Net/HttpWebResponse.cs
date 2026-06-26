// Lamella managed corlib (from scratch). -- System.Net.HttpWebResponse
#if LAMELLA_SURFACE_NET
using System.IO;

namespace System.Net
{
    public class HttpWebResponse : WebResponse
    {
        private HttpStatusCode _statusCode;
        private string _statusDescription;
        private WebHeaderCollection _headers;
        private Stream _bodyStream;
        private long _contentLength;
        private string _contentType;
        private Uri _responseUri;
        private HttpConnection _conn;

        internal HttpWebResponse()
        {
            _headers = new WebHeaderCollection();
            _contentLength = -1;
            _contentType = "";
        }

        public HttpStatusCode StatusCode { get { return _statusCode; } }
        public string StatusDescription { get { return _statusDescription; } }

        public Uri ResponseUri { get { return _responseUri; } }
        public override WebHeaderCollection Headers { get { return _headers; } }
        public override long ContentLength { get { return _contentLength; } }
        public override string ContentType { get { return _contentType; } }

        public override Stream GetResponseStream() { return _bodyStream; }

        public override void Close()
        {
            if (_conn != null)
            {
                _conn.Close();
                _conn = null;
            }
        }

        internal static HttpWebResponse ReadFrom(HttpConnection conn, string requestMethod, Uri responseUri)
        {
            HttpWebResponse r = new HttpWebResponse();
            r._conn = conn;
            r._responseUri = responseUri;

            string statusLine = conn.ReadLine();
            if ((object)statusLine == null)
                throw new WebException("The server committed a protocol violation.", WebExceptionStatus.ServerProtocolViolation);
            ParseStatusLine(statusLine, r);

            string line;
            while ((object)(line = conn.ReadLine()) != null && line.Length > 0)
            {
                int colon = line.IndexOf(':');
                if (colon < 0) continue;
                string name = line.Substring(0, colon).Trim();
                string value = line.Substring(colon + 1).Trim();
                r._headers.Add(name, value);
            }

            string contentType = r._headers["Content-Type"];
            if ((object)contentType != null) r._contentType = contentType;

            int status = (int)r._statusCode;
            bool noBody = requestMethod == "HEAD" || status == 204 || status == 304
                          || (status >= 100 && status < 200);

            string transferEncoding = r._headers["Transfer-Encoding"];
            bool chunked = (object)transferEncoding != null && ContainsChunked(transferEncoding);
            string contentLength = r._headers["Content-Length"];

            if (noBody)
            {
                r._contentLength = (object)contentLength != null ? long.Parse(contentLength) : 0;
                r._bodyStream = new LengthReadStream(conn, 0);
            }
            else if (chunked)
            {
                r._contentLength = -1;
                r._bodyStream = new ChunkedReadStream(conn);
            }
            else if ((object)contentLength != null)
            {
                long n = long.Parse(contentLength);
                r._contentLength = n;
                r._bodyStream = new LengthReadStream(conn, n);
            }
            else
            {
                r._contentLength = -1;
                r._bodyStream = new UntilCloseReadStream(conn);
            }
            return r;
        }

        private static bool ContainsChunked(string transferEncoding)
        {
            string haystack = transferEncoding.ToLower();
            string needle = "chunked";
            int limit = haystack.Length - needle.Length;
            for (int i = 0; i <= limit; i++)
            {
                bool match = true;
                for (int j = 0; j < needle.Length; j++)
                {
                    if (haystack[i + j] != needle[j]) { match = false; break; }
                }
                if (match) return true;
            }
            return false;
        }

        private static void ParseStatusLine(string s, HttpWebResponse r)
        {
            int sp1 = s.IndexOf(' ');
            if (sp1 < 0)
                throw new WebException("The server committed a protocol violation.", WebExceptionStatus.ServerProtocolViolation);
            int sp2 = s.IndexOf(' ', sp1 + 1);
            string codeText;
            string reason;
            if (sp2 < 0)
            {
                codeText = s.Substring(sp1 + 1);
                reason = "";
            }
            else
            {
                codeText = s.Substring(sp1 + 1, sp2 - (sp1 + 1));
                reason = s.Substring(sp2 + 1);
            }
            r._statusCode = (HttpStatusCode)int.Parse(codeText.Trim());
            r._statusDescription = reason;
        }
    }
}
#endif
