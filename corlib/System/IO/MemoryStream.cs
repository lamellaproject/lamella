// Lamella managed corlib (from scratch). -- System.IO.MemoryStream
namespace System.IO
{
    public class MemoryStream : Stream
    {
        private byte[] _buffer;
        private int _length;
        private int _position;
        private int _origin;
        private bool _expandable;
        private bool _writable;
        private bool _isOpen;

        public MemoryStream()
        {
            _buffer = new byte[0];
            _length = 0;
            _position = 0;
            _origin = 0;
            _expandable = true;
            _writable = true;
            _isOpen = true;
        }

        public MemoryStream(int capacity)
        {
            if (capacity < 0) throw new ArgumentOutOfRangeException("capacity");
            _buffer = new byte[capacity];
            _length = 0;
            _position = 0;
            _origin = 0;
            _expandable = true;
            _writable = true;
            _isOpen = true;
        }

        public MemoryStream(byte[] buffer)
        {
            if ((object)buffer == null) throw new ArgumentNullException("buffer");
            _buffer = buffer;
            _length = buffer.Length;
            _position = 0;
            _origin = 0;
            _expandable = false;
            _writable = true;
            _isOpen = true;
        }

        public MemoryStream(byte[] buffer, bool writable)
        {
            if ((object)buffer == null) throw new ArgumentNullException("buffer");
            _buffer = buffer;
            _length = buffer.Length;
            _position = 0;
            _origin = 0;
            _expandable = false;
            _writable = writable;
            _isOpen = true;
        }

        public override bool CanRead { get { return _isOpen; } }
        public override bool CanSeek { get { return _isOpen; } }
        public override bool CanWrite { get { return _isOpen && _writable; } }

        private void EnsureOpen()
        {
            if (!_isOpen) throw new ObjectDisposedException("MemoryStream", "Cannot access a closed Stream.");
        }

        private void EnsureWritable()
        {
            if (!_writable) throw new NotSupportedException("Stream does not support writing.");
        }

        public int Capacity
        {
            get
            {
                EnsureOpen();
                return _buffer.Length - _origin;
            }
            set
            {
                EnsureOpen();
                if (value < _length) throw new ArgumentOutOfRangeException("value");
                if (value == _buffer.Length - _origin) return;
                if (!_expandable) throw new NotSupportedException("Memory stream is not expandable.");
                if (value > 0)
                {
                    byte[] grown = new byte[value];
                    if (_length > 0) Buffer.BlockCopy(_buffer, _origin, grown, 0, _length);
                    _buffer = grown;
                }
                else
                {
                    _buffer = new byte[0];
                }
                _origin = 0;
            }
        }

        public override long Length
        {
            get
            {
                EnsureOpen();
                return _length;
            }
        }

        public override long Position
        {
            get
            {
                EnsureOpen();
                return _position;
            }
            set
            {
                EnsureOpen();
                if (value < 0) throw new ArgumentOutOfRangeException("value");
                if (value > 2147483647) throw new ArgumentOutOfRangeException("value");
                _position = (int)value;
            }
        }

        private void EnsureCapacity(int value)
        {
            if (value <= _buffer.Length - _origin) return;
            int newCapacity = value;
            if (newCapacity < 256) newCapacity = 256;
            int doubled = (_buffer.Length - _origin) * 2;
            if (newCapacity < doubled) newCapacity = doubled;
            int oldOrigin = _origin;
            byte[] grown = new byte[newCapacity];
            if (_length > 0) Buffer.BlockCopy(_buffer, oldOrigin, grown, 0, _length);
            _buffer = grown;
            _origin = 0;
        }

        public override int Read(byte[] buffer, int offset, int count)
        {
            EnsureOpen();
            if ((object)buffer == null) throw new ArgumentNullException("buffer");
            if (offset < 0) throw new ArgumentOutOfRangeException("offset");
            if (count < 0) throw new ArgumentOutOfRangeException("count");
            if (buffer.Length - offset < count) throw new ArgumentException("Offset and length were out of bounds for the array or count is greater than the number of elements from index to the end of the source collection.");

            int available = _length - _position;
            if (available <= 0) return 0;
            int n = available < count ? available : count;
            Buffer.BlockCopy(_buffer, _origin + _position, buffer, offset, n);
            _position = _position + n;
            return n;
        }

        public override int ReadByte()
        {
            EnsureOpen();
            if (_position >= _length) return -1;
            byte b = _buffer[_origin + _position];
            _position = _position + 1;
            return b;
        }

        public override void Write(byte[] buffer, int offset, int count)
        {
            EnsureOpen();
            EnsureWritable();
            if ((object)buffer == null) throw new ArgumentNullException("buffer");
            if (offset < 0) throw new ArgumentOutOfRangeException("offset");
            if (count < 0) throw new ArgumentOutOfRangeException("count");
            if (buffer.Length - offset < count) throw new ArgumentException("Offset and length were out of bounds for the array or count is greater than the number of elements from index to the end of the source collection.");

            int end = _position + count;
            if (end < 0) throw new IOException("Stream was too long.");

            if (end > _length)
            {
                if (end > _buffer.Length - _origin)
                {
                    if (!_expandable) throw new NotSupportedException("Memory stream is not expandable.");
                    EnsureCapacity(end);
                }
                _length = end;
            }
            if (count > 0) Buffer.BlockCopy(buffer, offset, _buffer, _origin + _position, count);
            _position = end;
        }

        public override void WriteByte(byte value)
        {
            EnsureOpen();
            EnsureWritable();
            int end = _position + 1;
            if (end > _length)
            {
                if (end > _buffer.Length - _origin)
                {
                    if (!_expandable) throw new NotSupportedException("Memory stream is not expandable.");
                    EnsureCapacity(end);
                }
                _length = end;
            }
            _buffer[_origin + _position] = value;
            _position = end;
        }

        public override long Seek(long offset, SeekOrigin origin)
        {
            EnsureOpen();
            if (offset > 2147483647) throw new ArgumentOutOfRangeException("offset");

            long target;
            if (origin == SeekOrigin.Begin)
            {
                target = offset;
            }
            else if (origin == SeekOrigin.Current)
            {
                target = (long)_position + offset;
            }
            else if (origin == SeekOrigin.End)
            {
                target = (long)_length + offset;
            }
            else
            {
                throw new ArgumentException("Invalid seek origin.");
            }

            if (target < 0) throw new IOException("An attempt was made to move the position before the beginning of the stream.");
            _position = (int)target;
            return _position;
        }

        public override void SetLength(long value)
        {
            EnsureOpen();
            EnsureWritable();
            if (value < 0) throw new ArgumentOutOfRangeException("value");
            if (value > 2147483647) throw new ArgumentOutOfRangeException("value");
            int newLength = (int)value;

            if (newLength > _length)
            {
                if (newLength > _buffer.Length - _origin)
                {
                    if (!_expandable) throw new NotSupportedException("Memory stream is not expandable.");
                    EnsureCapacity(newLength);
                }
                for (int i = _length; i < newLength; i++) _buffer[_origin + i] = 0;
            }
            _length = newLength;
            if (_position > newLength) _position = newLength;
        }

        public byte[] ToArray()
        {
            EnsureOpen();
            byte[] copy = new byte[_length];
            if (_length > 0) Buffer.BlockCopy(_buffer, _origin, copy, 0, _length);
            return copy;
        }

        public byte[] GetBuffer()
        {
            EnsureOpen();
            return _buffer;
        }

        public void WriteTo(Stream stream)
        {
            EnsureOpen();
            if ((object)stream == null) throw new ArgumentNullException("stream");
            stream.Write(_buffer, _origin, _length);
        }

        public override void Flush()
        {
            EnsureOpen();
        }

        protected override void Dispose(bool disposing)
        {
            _isOpen = false;
        }
    }
}
