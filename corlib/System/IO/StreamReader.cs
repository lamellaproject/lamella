// Lamella managed corlib (from scratch). -- System.IO.StreamReader
namespace System.IO
{
    public class StreamReader : TextReader
    {
        private Stream _stream;
        private System.Text.Encoding _encoding;

        private byte[] _byteBuffer;
        private int _byteStart;
        private int _byteLen;

        private char[] _charBuffer;
        private int _charPos;
        private int _charLen;

        private bool _streamAtEnd;

        private bool _checkedPreamble;

        private bool _isOpen;

        public StreamReader(Stream stream) : this(stream, System.Text.Encoding.UTF8)
        {
        }

        public StreamReader(Stream stream, System.Text.Encoding encoding)
        {
            if ((object)stream == null) throw new ArgumentNullException("stream");
            if ((object)encoding == null) throw new ArgumentNullException("encoding");
            if (!stream.CanRead) throw new ArgumentException("Stream was not readable.");
            _stream = stream;
            _encoding = encoding;
            _byteBuffer = new byte[1024];
            _byteStart = 0;
            _byteLen = 0;
            _charBuffer = new char[0];
            _charPos = 0;
            _charLen = 0;
            _streamAtEnd = false;
            _checkedPreamble = false;
            _isOpen = true;
        }

        public virtual Stream BaseStream
        {
            get { return _stream; }
        }

        public virtual System.Text.Encoding CurrentEncoding
        {
            get { return _encoding; }
        }

        private void EnsureOpen()
        {
            if (!_isOpen) throw new ObjectDisposedException("StreamReader", "Cannot read from a closed TextReader.");
        }

        private static int CompleteByteCount(byte[] buffer, int start, int length)
        {
            int i = 0;
            while (i < length)
            {
                int lead = buffer[start + i];
                int seq;
                if (lead < 0x80) seq = 1;
                else if (lead < 0xC0) seq = 1;
                else if (lead < 0xE0) seq = 2;
                else if (lead < 0xF0) seq = 3;
                else seq = 4;
                if (i + seq > length)
                {
                    return i;
                }
                i = i + seq;
            }
            return i;
        }

        private void ConsumePreambleIfPresent()
        {
            if (_checkedPreamble) return;
            _checkedPreamble = true;
            if (_byteLen - _byteStart >= 3
                && _byteBuffer[_byteStart] == 0xEF
                && _byteBuffer[_byteStart + 1] == 0xBB
                && _byteBuffer[_byteStart + 2] == 0xBF)
            {
                _byteStart = _byteStart + 3;
            }
        }

        private bool FillByteBuffer()
        {
            if (_byteStart > 0)
            {
                int live = _byteLen - _byteStart;
                for (int i = 0; i < live; i++) _byteBuffer[i] = _byteBuffer[_byteStart + i];
                _byteStart = 0;
                _byteLen = live;
            }
            if (_byteLen == _byteBuffer.Length)
            {
                byte[] grown = new byte[_byteBuffer.Length * 2];
                for (int i = 0; i < _byteLen; i++) grown[i] = _byteBuffer[i];
                _byteBuffer = grown;
            }
            int read = _stream.Read(_byteBuffer, _byteLen, _byteBuffer.Length - _byteLen);
            if (read == 0)
            {
                _streamAtEnd = true;
                return false;
            }
            _byteLen = _byteLen + read;
            return true;
        }

        private int FillCharBuffer()
        {
            _charPos = 0;
            _charLen = 0;
            while (true)
            {
                int available = _byteLen - _byteStart;
                if (available == 0)
                {
                    if (_streamAtEnd) return 0;
                    if (!FillByteBuffer()) { if (_byteLen - _byteStart == 0) return 0; }
                    ConsumePreambleIfPresent();
                    continue;
                }

                ConsumePreambleIfPresent();
                available = _byteLen - _byteStart;
                if (available == 0)
                {
                    if (_streamAtEnd) return 0;
                    continue;
                }

                int complete = CompleteByteCount(_byteBuffer, _byteStart, available);
                if (complete == 0)
                {
                    if (!_streamAtEnd && FillByteBuffer()) continue;
                    complete = available;
                }

                byte[] run = new byte[complete];
                for (int i = 0; i < complete; i++) run[i] = _byteBuffer[_byteStart + i];
                _byteStart = _byteStart + complete;
                string decoded = _encoding.GetString(run);
                if (decoded.Length == 0)
                {
                    continue;
                }
                _charBuffer = new char[decoded.Length];
                for (int i = 0; i < decoded.Length; i++) _charBuffer[i] = decoded[i];
                _charLen = decoded.Length;
                return _charLen;
            }
        }

        private bool EnsureChars()
        {
            if (_charPos < _charLen) return true;
            return FillCharBuffer() > 0;
        }

        public override int Peek()
        {
            EnsureOpen();
            if (!EnsureChars()) return -1;
            return _charBuffer[_charPos];
        }

        public override int Read()
        {
            EnsureOpen();
            if (!EnsureChars()) return -1;
            char c = _charBuffer[_charPos];
            _charPos = _charPos + 1;
            return c;
        }

        public override int Read(char[] buffer, int index, int count)
        {
            EnsureOpen();
            if ((object)buffer == null) throw new ArgumentNullException("buffer");
            if (index < 0) throw new ArgumentOutOfRangeException("index");
            if (count < 0) throw new ArgumentOutOfRangeException("count");
            if (buffer.Length - index < count) throw new ArgumentException("Offset and length were out of bounds for the array or count is greater than the number of elements from index to the end of the source collection.");

            int read = 0;
            while (read < count)
            {
                if (!EnsureChars()) break;
                int take = _charLen - _charPos;
                int want = count - read;
                if (take > want) take = want;
                for (int i = 0; i < take; i++) buffer[index + read + i] = _charBuffer[_charPos + i];
                _charPos = _charPos + take;
                read = read + take;
            }
            return read;
        }

        public override string ReadLine()
        {
            EnsureOpen();
            if (!EnsureChars()) return null;
            System.Text.StringBuilder sb = new System.Text.StringBuilder();
            while (true)
            {
                if (!EnsureChars())
                {
                    return sb.ToString();
                }
                char c = _charBuffer[_charPos];
                _charPos = _charPos + 1;
                if (c == '\n')
                {
                    return sb.ToString();
                }
                if (c == '\r')
                {
                    if (EnsureChars() && _charBuffer[_charPos] == '\n') _charPos = _charPos + 1;
                    return sb.ToString();
                }
                sb.Append(c);
            }
        }

        public override string ReadToEnd()
        {
            EnsureOpen();
            System.Text.StringBuilder sb = new System.Text.StringBuilder();
            while (EnsureChars())
            {
                for (int i = _charPos; i < _charLen; i++) sb.Append(_charBuffer[i]);
                _charPos = _charLen;
            }
            return sb.ToString();
        }

        public bool EndOfStream
        {
            get
            {
                EnsureOpen();
                if (_charPos < _charLen) return false;
                return FillCharBuffer() == 0;
            }
        }

        public override void Close()
        {
            Dispose(true);
        }

        protected override void Dispose(bool disposing)
        {
            if (_isOpen && disposing)
            {
                _stream.Close();
            }
            _isOpen = false;
        }
    }
}
