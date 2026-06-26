// Lamella managed corlib (from scratch). -- System.Net.HttpConnection
#if LAMELLA_SURFACE_NET
using System.IO;
using System.Net.Sockets;
using System.Text;

namespace System.Net
{
    internal class HttpConnection
    {
        private Stream _stream;
        private TcpClient _client;
        private byte[] _buf;
        private int _pos;
        private int _len;

        internal HttpConnection(Stream stream, TcpClient client)
        {
            _stream = stream;
            _client = client;
            _buf = new byte[4096];
            _pos = 0;
            _len = 0;
        }

        private bool Fill()
        {
            _pos = 0;
            _len = _stream.Read(_buf, 0, _buf.Length);
            return _len > 0;
        }

        internal int ReadByteRaw()
        {
            if (_pos >= _len)
            {
                if (!Fill()) return -1;
            }
            return _buf[_pos++];
        }

        internal int ReadRaw(byte[] dst, int offset, int count)
        {
            if (count <= 0) return 0;
            if (_pos >= _len)
            {
                if (!Fill()) return 0;
            }
            int available = _len - _pos;
            int n = available < count ? available : count;
            Buffer.BlockCopy(_buf, _pos, dst, offset, n);
            _pos += n;
            return n;
        }

        internal string ReadLine()
        {
            StringBuilder sb = new StringBuilder();
            bool any = false;
            int c;
            while ((c = ReadByteRaw()) >= 0)
            {
                any = true;
                if (c == 13)
                {
                    int next = ReadByteRaw();
                    if (next == 10) break;
                    sb.Append((char)13);
                    if (next >= 0) sb.Append((char)next);
                    continue;
                }
                if (c == 10) break;
                sb.Append((char)c);
            }
            if (!any) return null;
            return sb.ToString();
        }

        internal void Close()
        {
            _stream.Close();
            if (_client != null) _client.Close();
        }
    }
}
#endif
