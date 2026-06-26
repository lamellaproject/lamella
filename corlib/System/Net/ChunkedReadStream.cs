// Lamella managed corlib (from scratch). -- System.Net.ChunkedReadStream
#if LAMELLA_SURFACE_NET
using System.IO;

namespace System.Net
{
    internal class ChunkedReadStream : Stream
    {
        private HttpConnection _conn;
        private long _chunkRemaining;
        private bool _done;

        internal ChunkedReadStream(HttpConnection conn)
        {
            _conn = conn;
            _chunkRemaining = 0;
            _done = false;
        }

        public override bool CanRead { get { return true; } }
        public override bool CanWrite { get { return false; } }
        public override bool CanSeek { get { return false; } }
        public override long Length { get { throw new NotSupportedException(); } }
        public override long Position
        {
            get { throw new NotSupportedException(); }
            set { throw new NotSupportedException(); }
        }

        public override int Read(byte[] buffer, int offset, int count)
        {
            if ((object)buffer == null) throw new ArgumentNullException("buffer");
            if (_done || count <= 0) return 0;
            if (_chunkRemaining == 0)
            {
                if (!NextChunk()) return 0;
            }
            long want = count;
            if (want > _chunkRemaining) want = _chunkRemaining;
            int n = _conn.ReadRaw(buffer, offset, (int)want);
            _chunkRemaining -= n;
            if (_chunkRemaining == 0)
            {
                _conn.ReadByteRaw();
                _conn.ReadByteRaw();
            }
            return n;
        }

        private bool NextChunk()
        {
            string sizeLine = _conn.ReadLine();
            if ((object)sizeLine == null) { _done = true; return false; }
            int semi = sizeLine.IndexOf(';');
            string hex = semi >= 0 ? sizeLine.Substring(0, semi) : sizeLine;
            long size = ParseHex(hex.Trim());
            if (size == 0)
            {
                string trailer;
                while ((object)(trailer = _conn.ReadLine()) != null && trailer.Length > 0) { }
                _done = true;
                return false;
            }
            _chunkRemaining = size;
            return true;
        }

        private static long ParseHex(string s)
        {
            long value = 0;
            for (int i = 0; i < s.Length; i++)
            {
                char c = s[i];
                int digit;
                if (c >= '0' && c <= '9') digit = c - '0';
                else if (c >= 'a' && c <= 'f') digit = c - 'a' + 10;
                else if (c >= 'A' && c <= 'F') digit = c - 'A' + 10;
                else break;
                value = value * 16 + digit;
            }
            return value;
        }

        public override void Write(byte[] buffer, int offset, int count) { throw new NotSupportedException(); }
        public override long Seek(long offset, SeekOrigin origin) { throw new NotSupportedException(); }
        public override void SetLength(long value) { throw new NotSupportedException(); }
        public override void Flush() { }
    }
}
#endif
