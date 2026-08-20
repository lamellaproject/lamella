// System.IO.Ports (libs/, real .NET's own assembly name) -- System.IO.Ports.SerialStream
#if LAMELLA_SURFACE_SERIAL && LAMELLA_SURFACE_NETFX_2_0
namespace System.IO.Ports
{
    internal sealed class SerialStream : Stream
    {
        private SerialPort _port;

        internal SerialStream(SerialPort port)
        {
            _port = port;
        }

        private SerialPort Live()
        {
            if (_port == null) throw new ObjectDisposedException("SerialStream", "Cannot access a closed Stream.");
            return _port;
        }

        public override bool CanRead { get { return true; } }
        public override bool CanWrite { get { return true; } }
        public override bool CanSeek { get { return false; } }
        public bool CanTimeout { get { return true; } }

        public override int Read(byte[] buffer, int offset, int count)
        {
            return Live().Read(buffer, offset, count);
        }

        public override void Write(byte[] buffer, int offset, int count)
        {
            Live().Write(buffer, offset, count);
        }

        public override void Flush()
        {
            Live().FlushPort();
        }

        public override long Length { get { throw new NotSupportedException("Seek is not supported on a serial port."); } }

        public override long Position
        {
            get { throw new NotSupportedException("Seek is not supported on a serial port."); }
            set { throw new NotSupportedException("Seek is not supported on a serial port."); }
        }

        public override long Seek(long offset, SeekOrigin origin)
        {
            throw new NotSupportedException("Seek is not supported on a serial port.");
        }

        public override void SetLength(long value)
        {
            throw new NotSupportedException("Seek is not supported on a serial port.");
        }

        protected override void Dispose(bool disposing)
        {
            _port = null;
            base.Dispose(disposing);
        }
    }
}
#endif
