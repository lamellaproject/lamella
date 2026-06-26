// Lamella managed corlib (from scratch). -- System.Net.Sockets.NetworkStream
#if LAMELLA_SURFACE_NET
using System.IO;

namespace System.Net.Sockets
{
    public class NetworkStream : Stream
    {
        private Socket _socket;

        public NetworkStream(Socket socket) { _socket = socket; }

        public override bool CanRead { get { return true; } }
        public override bool CanWrite { get { return true; } }
        public override bool CanSeek { get { return false; } }
        public override long Length { get { throw new NotSupportedException(); } }
        public override long Position
        {
            get { throw new NotSupportedException(); }
            set { throw new NotSupportedException(); }
        }

        public override int Read(byte[] buffer, int offset, int count)
        {
            return _socket.Receive(buffer, offset, count, SocketFlags.None);
        }

        public override void Write(byte[] buffer, int offset, int count)
        {
            _socket.Send(buffer, offset, count, SocketFlags.None);
        }

        public override long Seek(long offset, SeekOrigin origin) { throw new NotSupportedException(); }
        public override void SetLength(long value) { throw new NotSupportedException(); }
        public override void Flush() { }
        public override void Close() { _socket.Close(); }
    }
}
#endif
