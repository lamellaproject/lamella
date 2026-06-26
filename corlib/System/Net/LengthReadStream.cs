// Lamella managed corlib (from scratch). -- System.Net.LengthReadStream
#if LAMELLA_SURFACE_NET
using System.IO;

namespace System.Net
{
    internal class LengthReadStream : Stream
    {
        private HttpConnection _conn;
        private long _remaining;

        internal LengthReadStream(HttpConnection conn, long length)
        {
            _conn = conn;
            _remaining = length;
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
            if (_remaining <= 0 || count <= 0) return 0;
            long want = count;
            if (want > _remaining) want = _remaining;
            int n = _conn.ReadRaw(buffer, offset, (int)want);
            _remaining -= n;
            return n;
        }

        public override void Write(byte[] buffer, int offset, int count) { throw new NotSupportedException(); }
        public override long Seek(long offset, SeekOrigin origin) { throw new NotSupportedException(); }
        public override void SetLength(long value) { throw new NotSupportedException(); }
        public override void Flush() { }
    }
}
#endif
