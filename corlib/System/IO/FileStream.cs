// Lamella managed corlib (from scratch). -- System.IO.FileStream
#if LAMELLA_SURFACE_FILE_IO
namespace System.IO
{
    public class FileStream : Stream
    {
        private int _handle;
        private string _path;
        private FileAccess _access;
        private long _appendStart;

        public FileStream(string path, FileMode mode)
            : this(path, mode, mode == FileMode.Append ? FileAccess.Write : FileAccess.ReadWrite)
        {
        }

        public FileStream(string path, FileMode mode, FileAccess access)
        {
            if ((object)path == null) throw new ArgumentNullException("path");
            if (path.Length == 0) throw new ArgumentException("Empty path name is not legal.");
            if (mode == FileMode.Append && access != FileAccess.Write)
                throw new ArgumentException("Append access can be requested only in write-only mode.");
            int handle = NativeFs.Open(path, (int)mode, (int)access);
            if (handle < 0) NativeFs.Throw(handle, path);
            _handle = handle;
            _path = path;
            _access = access;
            _appendStart = -1;
            if (mode == FileMode.Append)
            {
                _appendStart = NativeFs.Seek(handle, 0, (int)SeekOrigin.Current);
                if (_appendStart < 0) _appendStart = 0;
            }
        }

        public string Name { get { return _path; } }

        public override bool CanRead { get { return _handle >= 0 && _access != FileAccess.Write; } }
        public override bool CanWrite { get { return _handle >= 0 && _access != FileAccess.Read; } }
        public override bool CanSeek { get { return _handle >= 0; } }

        public override long Length
        {
            get
            {
                EnsureOpen();
                long length = NativeFs.Length(_handle);
                if (length < 0) NativeFs.Throw((int)length, _path);
                return length;
            }
        }

        public override long Position
        {
            get
            {
                EnsureOpen();
                long position = NativeFs.Seek(_handle, 0, (int)SeekOrigin.Current);
                if (position < 0) NativeFs.Throw((int)position, _path);
                return position;
            }
            set
            {
                if (value < 0) throw new ArgumentOutOfRangeException("value");
                Seek(value, SeekOrigin.Begin);
            }
        }

        public override int Read(byte[] buffer, int offset, int count)
        {
            ValidateRange(buffer, offset, count);
            EnsureOpen();
            if (!CanRead) throw new NotSupportedException("Stream does not support reading.");
            int read = NativeFs.Read(_handle, buffer, offset, count);
            if (read < 0) NativeFs.Throw(read, _path);
            return read;
        }

        public override void Write(byte[] buffer, int offset, int count)
        {
            ValidateRange(buffer, offset, count);
            EnsureOpen();
            if (!CanWrite) throw new NotSupportedException("Stream does not support writing.");
            int written = 0;
            while (written < count)
            {
                int n = NativeFs.Write(_handle, buffer, offset + written, count - written);
                if (n < 0) NativeFs.Throw(n, _path);
                if (n == 0) throw new IOException("An I/O error occurred while accessing '" + _path + "'.");
                written += n;
            }
        }

        public override long Seek(long offset, SeekOrigin origin)
        {
            EnsureOpen();
            long position = NativeFs.Seek(_handle, offset, (int)origin);
            if (position < 0) NativeFs.Throw((int)position, _path);
            if (_appendStart >= 0 && position < _appendStart)
            {
                NativeFs.Seek(_handle, _appendStart, (int)SeekOrigin.Begin);
                throw new IOException("Unable to seek to a position before the append start.");
            }
            return position;
        }

        public override void SetLength(long value)
        {
            if (value < 0) throw new ArgumentOutOfRangeException("value");
            EnsureOpen();
            if (!CanWrite) throw new NotSupportedException("Stream does not support writing.");
            int code = NativeFs.SetLength(_handle, value);
            if (code < 0) NativeFs.Throw(code, _path);
        }

        public override void Flush()
        {
            EnsureOpen();
            int code = NativeFs.Flush(_handle);
            if (code < 0) NativeFs.Throw(code, _path);
        }

        public override void Close()
        {
            if (_handle >= 0)
            {
                NativeFs.Close(_handle);
                _handle = -1;
            }
        }

        private void EnsureOpen()
        {
            if (_handle < 0) throw new ObjectDisposedException("FileStream");
        }

        private static void ValidateRange(byte[] buffer, int offset, int count)
        {
            if ((object)buffer == null) throw new ArgumentNullException("buffer");
            if (offset < 0) throw new ArgumentOutOfRangeException("offset");
            if (count < 0) throw new ArgumentOutOfRangeException("count");
            if (buffer.Length - offset < count)
                throw new ArgumentException("Offset and length were out of bounds for the array or count is greater than the number of elements from index to the end of the source collection.");
        }
    }
}
#endif
