// Lamella managed corlib (from scratch). -- System.Net.HttpRequestStream
#if LAMELLA_SURFACE_NET
using System.IO;

namespace System.Net
{
    internal class HttpRequestStream : Stream
    {
        private MemoryStream _buffer;

        internal HttpRequestStream()
        {
            _buffer = new MemoryStream();
        }

        public override bool CanRead { get { return false; } }
        public override bool CanWrite { get { return true; } }
        public override bool CanSeek { get { return false; } }
        public override long Length { get { return _buffer.Length; } }
        public override long Position
        {
            get { return _buffer.Position; }
            set { throw new NotSupportedException(); }
        }

        public override int Read(byte[] buffer, int offset, int count) { throw new NotSupportedException(); }

        public override void Write(byte[] buffer, int offset, int count)
        {
            _buffer.Write(buffer, offset, count);
        }

        public override void WriteByte(byte value) { _buffer.WriteByte(value); }

        public override long Seek(long offset, SeekOrigin origin) { throw new NotSupportedException(); }
        public override void SetLength(long value) { throw new NotSupportedException(); }
        public override void Flush() { }

        public override void Close() { }
        protected override void Dispose(bool disposing) { }

        internal byte[] ToBytes() { return _buffer.ToArray(); }
    }
}
#endif
