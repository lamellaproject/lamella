// Lamella managed corlib (from scratch). -- System.IO.StreamWriter
namespace System.IO
{
    public class StreamWriter : TextWriter
    {
        private Stream _stream;
        private System.Text.Encoding _encoding;

        private string _newLine;

        private bool _autoFlush;

        private bool _isOpen;

        public StreamWriter(Stream stream) : this(stream, System.Text.Encoding.UTF8)
        {
        }

        public StreamWriter(Stream stream, System.Text.Encoding encoding)
        {
            if ((object)stream == null) throw new ArgumentNullException("stream");
            if ((object)encoding == null) throw new ArgumentNullException("encoding");
            if (!stream.CanWrite) throw new ArgumentException("Stream was not writable.");
            _stream = stream;
            _encoding = encoding;
            _newLine = "\r\n";
            _autoFlush = false;
            _isOpen = true;
        }

        public virtual Stream BaseStream
        {
            get { return _stream; }
        }

        public virtual System.Text.Encoding Encoding
        {
            get { return _encoding; }
        }

        public override string NewLine
        {
            get { return _newLine; }
            set { _newLine = (object)value == null ? "\r\n" : value; }
        }

        public virtual bool AutoFlush
        {
            get { return _autoFlush; }
            set
            {
                EnsureOpen();
                _autoFlush = value;
                if (value) _stream.Flush();
            }
        }

        private void EnsureOpen()
        {
            if (!_isOpen) throw new ObjectDisposedException("StreamWriter", "Cannot write to a closed TextWriter.");
        }

        private void WriteText(string value)
        {
            if ((object)value == null || value.Length == 0) return;
            byte[] bytes = _encoding.GetBytes(value);
            _stream.Write(bytes, 0, bytes.Length);
            if (_autoFlush) _stream.Flush();
        }

        public override void Write(char value)
        {
            EnsureOpen();
            System.Text.StringBuilder one = new System.Text.StringBuilder();
            one.Append(value);
            WriteText(one.ToString());
        }

        public override void Write(string value)
        {
            EnsureOpen();
            WriteText(value);
        }

        public virtual void Write(char[] buffer)
        {
            EnsureOpen();
            if ((object)buffer == null) throw new ArgumentNullException("buffer");
            System.Text.StringBuilder sb = new System.Text.StringBuilder();
            for (int i = 0; i < buffer.Length; i++) sb.Append(buffer[i]);
            WriteText(sb.ToString());
        }

        public virtual void Write(char[] buffer, int index, int count)
        {
            EnsureOpen();
            if ((object)buffer == null) throw new ArgumentNullException("buffer");
            if (index < 0) throw new ArgumentOutOfRangeException("index");
            if (count < 0) throw new ArgumentOutOfRangeException("count");
            if (buffer.Length - index < count) throw new ArgumentException("Offset and length were out of bounds for the array or count is greater than the number of elements from index to the end of the source collection.");
            System.Text.StringBuilder sb = new System.Text.StringBuilder();
            for (int i = 0; i < count; i++) sb.Append(buffer[index + i]);
            WriteText(sb.ToString());
        }

        public override void WriteLine()
        {
            EnsureOpen();
            WriteText(_newLine);
        }

        public override void WriteLine(string value)
        {
            EnsureOpen();
            WriteText(value);
            WriteText(_newLine);
        }

        public override void WriteLine(char value)
        {
            EnsureOpen();
            System.Text.StringBuilder one = new System.Text.StringBuilder();
            one.Append(value);
            WriteText(one.ToString());
            WriteText(_newLine);
        }

        public override void Flush()
        {
            EnsureOpen();
            _stream.Flush();
        }

        public override void Close()
        {
            Dispose(true);
        }

        protected override void Dispose(bool disposing)
        {
            if (_isOpen && disposing)
            {
                _stream.Flush();
                _stream.Close();
            }
            _isOpen = false;
        }
    }
}
