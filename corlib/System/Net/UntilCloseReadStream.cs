// Lamella managed corlib (from scratch). -- System.Net.UntilCloseReadStream
#if LAMELLA_SURFACE_NET
using System.IO;

namespace System.Net
{
    internal class UntilCloseReadStream : Stream
    {
        private HttpConnection _conn;
        private bool _eof;

        internal UntilCloseReadStream(HttpConnection conn)
        {
            _conn = conn;
            _eof = false;
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
            if (_eof || count <= 0) return 0;
            int n = _conn.ReadRaw(buffer, offset, count);
            if (n == 0) _eof = true;
            return n;
        }

        public override void Write(byte[] buffer, int offset, int count) { throw new NotSupportedException(); }
        public override long Seek(long offset, SeekOrigin origin) { throw new NotSupportedException(); }
        public override void SetLength(long value) { throw new NotSupportedException(); }
        public override void Flush() { }
    }
}
#endif
