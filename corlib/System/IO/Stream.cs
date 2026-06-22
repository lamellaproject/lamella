// Lamella managed corlib (from scratch). -- System.IO.Stream
namespace System.IO
{
    public abstract class Stream : IDisposable
    {
        public abstract bool CanRead { get; }
        public abstract bool CanWrite { get; }
        public abstract bool CanSeek { get; }

        public abstract long Length { get; }
        public abstract long Position { get; set; }

        public abstract int Read(byte[] buffer, int offset, int count);
        public abstract void Write(byte[] buffer, int offset, int count);
        public abstract long Seek(long offset, SeekOrigin origin);
        public abstract void SetLength(long value);
        public abstract void Flush();

        public virtual int ReadByte()
        {
            byte[] one = new byte[1];
            int read = Read(one, 0, 1);
            if (read == 0) return -1;
            return one[0];
        }

        public virtual void WriteByte(byte value)
        {
            byte[] one = new byte[1];
            one[0] = value;
            Write(one, 0, 1);
        }

        public virtual void Close()
        {
            Dispose(true);
        }

        public void Dispose()
        {
            Dispose(true);
        }

        protected virtual void Dispose(bool disposing)
        {
        }
    }
}
