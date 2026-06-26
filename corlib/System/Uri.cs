// Lamella managed corlib (from scratch). -- System.Uri
namespace System
{
    public class Uri
    {
        private string _original;
        private string _scheme;
        private string _host;
        private int _port;
        private string _path;
        private string _query;

        public Uri(string uriString)
        {
            if ((object)uriString == null) throw new ArgumentNullException("uriString");
            _original = uriString;
            Parse(uriString);
        }

        private void Parse(string s)
        {
            int colon = s.IndexOf(':');
            if (colon <= 0 || colon + 2 >= s.Length || s[colon + 1] != '/' || s[colon + 2] != '/')
                throw new UriFormatException("Invalid URI: The format of the URI could not be determined.");
            _scheme = s.Substring(0, colon).ToLower();

            int authStart = colon + 3;
            int authEnd = s.Length;
            for (int i = authStart; i < s.Length; i++)
            {
                char c = s[i];
                if (c == '/' || c == '?') { authEnd = i; break; }
            }
            string authority = s.Substring(authStart, authEnd - authStart);
            if (authority.Length == 0)
                throw new UriFormatException("Invalid URI: The hostname could not be parsed.");

            int hostColon = authority.IndexOf(':');
            if (hostColon >= 0)
            {
                _host = authority.Substring(0, hostColon);
                _port = int.Parse(authority.Substring(hostColon + 1));
            }
            else
            {
                _host = authority;
                _port = DefaultPort(_scheme);
            }

            string rest = authEnd < s.Length ? s.Substring(authEnd) : "";
            int q = rest.IndexOf('?');
            if (q >= 0)
            {
                _path = rest.Substring(0, q);
                _query = rest.Substring(q);
            }
            else
            {
                _path = rest;
                _query = "";
            }
            if (_path.Length == 0) _path = "/";
        }

        private static int DefaultPort(string scheme)
        {
            if (scheme == "http") return 80;
            if (scheme == "https") return 443;
            return -1;
        }

        public string Scheme { get { return _scheme; } }
        public string Host { get { return _host; } }
        public int Port { get { return _port; } }
        public string AbsolutePath { get { return _path; } }
        public string Query { get { return _query; } }
        public string PathAndQuery { get { return _path + _query; } }
        public string OriginalString { get { return _original; } }

        public bool IsDefaultPort { get { return _port == DefaultPort(_scheme); } }

        public override string ToString() { return _original; }
    }
}
