// System.IO.Ports (libs/, full .NET's own assembly name) -- System.IO.Ports.SerialPort
#if LAMELLA_SURFACE_SERIAL && LAMELLA_SURFACE_NETFX_2_0
namespace System.IO.Ports
{
    public class SerialPort
    {
        public const int InfiniteTimeout = -1;

        private string _portName;
        private int _baudRate;
        private Parity _parity;
        private int _dataBits;
        private StopBits _stopBits;
        private Handshake _handshake;
        private int _readTimeout;
        private int _writeTimeout;
        private int _handle;
        private Stream _baseStream;

        private string _newLine;
        private System.Text.Encoding _encoding;
        private byte[] _inBuffer;
        private int _readPos;
        private int _readLen;

        public SerialPort(string portName)
            : this(portName, 9600, Parity.None, 8, StopBits.One)
        {
        }

        public SerialPort(string portName, int baudRate)
            : this(portName, baudRate, Parity.None, 8, StopBits.One)
        {
        }

        public SerialPort(string portName, int baudRate, Parity parity)
            : this(portName, baudRate, parity, 8, StopBits.One)
        {
        }

        public SerialPort(string portName, int baudRate, Parity parity, int dataBits)
            : this(portName, baudRate, parity, dataBits, StopBits.One)
        {
        }

        public SerialPort(string portName, int baudRate, Parity parity, int dataBits, StopBits stopBits)
        {
            if ((object)portName == null) throw new ArgumentNullException("portName");
            if (portName.Length == 0) throw new ArgumentException("The PortName cannot be empty.", "portName");
            _portName = portName;
            _baudRate = baudRate;
            _parity = parity;
            _dataBits = dataBits;
            _stopBits = stopBits;
            _handshake = Handshake.None;
            _readTimeout = InfiniteTimeout;
            _writeTimeout = InfiniteTimeout;
            _handle = -1;
            _newLine = "\n";
            _encoding = System.Text.Encoding.ASCII;
        }

        public string PortName { get { return _portName; } }

        public bool IsOpen { get { return _handle >= 0; } }

        public int BaudRate
        {
            get { return _baudRate; }
            set
            {
                if (value <= 0) throw new ArgumentOutOfRangeException("value");
                EnsureNotOpen();
                _baudRate = value;
            }
        }

        public Parity Parity
        {
            get { return _parity; }
            set
            {
                if ((int)value < (int)Parity.None || (int)value > (int)Parity.Space)
                    throw new ArgumentOutOfRangeException("value");
                EnsureNotOpen();
                _parity = value;
            }
        }

        public int DataBits
        {
            get { return _dataBits; }
            set
            {
                if (value < 5 || value > 8) throw new ArgumentOutOfRangeException("value");
                EnsureNotOpen();
                _dataBits = value;
            }
        }

        public StopBits StopBits
        {
            get { return _stopBits; }
            set
            {
                if ((int)value < (int)StopBits.One || (int)value > (int)StopBits.OnePointFive)
                    throw new ArgumentOutOfRangeException("value");
                EnsureNotOpen();
                _stopBits = value;
            }
        }

        public Handshake Handshake
        {
            get { return _handshake; }
            set
            {
                if ((int)value < (int)Handshake.None || (int)value > (int)Handshake.RequestToSendXOnXOff)
                    throw new ArgumentOutOfRangeException("value");
                EnsureNotOpen();
                _handshake = value;
            }
        }

        public int ReadTimeout
        {
            get { return _readTimeout; }
            set
            {
                if (value < 0 && value != InfiniteTimeout)
                    throw new ArgumentOutOfRangeException("value");
                _readTimeout = value;
            }
        }

        public int WriteTimeout
        {
            get { return _writeTimeout; }
            set
            {
                if (value < 0 && value != InfiniteTimeout)
                    throw new ArgumentOutOfRangeException("value");
                _writeTimeout = value;
            }
        }

        public string NewLine
        {
            get { return _newLine; }
            set
            {
                if ((object)value == null) throw new ArgumentNullException("NewLine");
                if (value.Length == 0) throw new ArgumentException("Argument NewLine cannot be null or zero-length.", "NewLine");
                _newLine = value;
            }
        }

        public System.Text.Encoding Encoding
        {
            get { return _encoding; }
            set
            {
                if ((object)value == null) throw new ArgumentNullException("Encoding");
                _encoding = value;
            }
        }

        public int BytesToRead
        {
            get
            {
                EnsureOpen();
                int n = NativeSerial.BytesToRead(_handle);
                if (n < 0) NativeSerial.Throw(n, _portName);
                return n + (_readLen - _readPos);
            }
        }

        public int BytesToWrite
        {
            get
            {
                EnsureOpen();
                int n = NativeSerial.BytesToWrite(_handle);
                if (n < 0) NativeSerial.Throw(n, _portName);
                return n;
            }
        }

        public Stream BaseStream
        {
            get
            {
                EnsureOpen();
                if (_baseStream == null) _baseStream = new SerialStream(this);
                return _baseStream;
            }
        }

#if LAMELLA_SURFACE_THREADS

        /// <summary>Indicates that data has been received through a port represented by the
        /// <see cref="SerialPort"/> object.</summary>
        public event SerialDataReceivedEventHandler DataReceived;

        private const int PollIntervalMs = 10;

        private int _receivedBytesThreshold = 1;

        private int _pumpGeneration;

        /// <summary>The number of bytes in the internal input buffer before a
        /// <see cref="DataReceived"/> event occurs. The default is 1.</summary>
        public int ReceivedBytesThreshold
        {
            get { return _receivedBytesThreshold; }
            set
            {
                if (value <= 0) throw new ArgumentOutOfRangeException("value");
                _receivedBytesThreshold = value;
            }
        }

        private void RaiseDataReceived(SerialData eventType)
        {
            SerialDataReceivedEventHandler handlers = DataReceived;
            if (handlers != null)
            {
                handlers(this, new SerialDataReceivedEventArgs(eventType));
            }
        }

        private void StartPump()
        {
            _pumpGeneration++;
            System.Threading.Thread pump = new System.Threading.Thread(new System.Threading.ThreadStart(PumpLoop));
            pump.IsBackground = true;
            pump.Start();
        }

        private void PumpLoop()
        {
            int generation = _pumpGeneration;
            bool raised = false;
            while (true)
            {
                System.Threading.Thread.Sleep(PollIntervalMs);
                if (_pumpGeneration != generation) return;
                int handle = _handle;
                if (handle < 0) return;
                int available = NativeSerial.BytesToRead(handle);
                if (available < 0) return;
                if (available >= _receivedBytesThreshold)
                {
                    if (!raised)
                    {
                        raised = true;
                        RaiseDataReceived(SerialData.Chars);
                    }
                }
                else
                {
                    raised = false;
                }
            }
        }
#endif

        public void Open()
        {
            if (_handle >= 0) throw new InvalidOperationException("The port is already open.");
            int handle = NativeSerial.Open(_portName, _baudRate, (int)_parity, _dataBits, (int)_stopBits, (int)_handshake);
            if (handle < 0) NativeSerial.Throw(handle, _portName);
            _handle = handle;
#if LAMELLA_SURFACE_THREADS
            StartPump();
#endif
        }

        public int Read(byte[] buffer, int offset, int count)
        {
            EnsureOpen();
            ValidateRange(buffer, offset, count);
            int cached = _readLen - _readPos;
            if (cached > 0)
            {
                int taken = cached < count ? cached : count;
                Array.Copy(_inBuffer, _readPos, buffer, offset, taken);
                Consume(taken);
                if (taken == count) return taken;
                int more = NativeSerial.BytesToRead(_handle);
                if (more < 0) NativeSerial.Throw(more, _portName);
                if (more == 0) return taken;
                int rest = NativeSerial.Read(_handle, buffer, offset + taken, count - taken, 0);
                if (rest < 0) NativeSerial.Throw(rest, _portName);
                return taken + rest;
            }
            int read = NativeSerial.Read(_handle, buffer, offset, count, _readTimeout);
            if (read < 0) NativeSerial.Throw(read, _portName);
            return read;
        }

        public void Write(byte[] buffer, int offset, int count)
        {
            EnsureOpen();
            ValidateRange(buffer, offset, count);
            int written = 0;
            while (written < count)
            {
                int n = NativeSerial.Write(_handle, buffer, offset + written, count - written, _writeTimeout);
                if (n < 0) NativeSerial.Throw(n, _portName);
                if (n == 0) throw new IOException("An I/O error occurred while accessing the port '" + _portName + "'.");
                written += n;
            }
        }

        internal void FlushPort()
        {
            EnsureOpen();
            int code = NativeSerial.Flush(_handle);
            if (code < 0) NativeSerial.Throw(code, _portName);
        }

        public void DiscardInBuffer()
        {
            EnsureOpen();
            int code = NativeSerial.DiscardIn(_handle);
            if (code < 0) NativeSerial.Throw(code, _portName);
            _readPos = 0;
            _readLen = 0;
        }

        public void DiscardOutBuffer()
        {
            EnsureOpen();
            int code = NativeSerial.DiscardOut(_handle);
            if (code < 0) NativeSerial.Throw(code, _portName);
        }


        public void Write(string text)
        {
            EnsureOpen();
            if ((object)text == null) throw new ArgumentNullException("text");
            if (text.Length == 0) return;
            byte[] bytes = _encoding.GetBytes(text);
            Write(bytes, 0, bytes.Length);
        }

        public void Write(char[] buffer, int offset, int count)
        {
            EnsureOpen();
            if ((object)buffer == null) throw new ArgumentNullException("buffer");
            ValidateRange(buffer.Length, offset, count);
            if (count == 0) return;
            Write(new string(buffer, offset, count));
        }

        public void WriteLine(string text)
        {
            Write(text + _newLine);
        }

        public int ReadByte()
        {
            EnsureOpen();
            if (_readLen == _readPos && Fill(_readTimeout) == 0) throw new TimeoutException();
            int b = _inBuffer[_readPos];
            Consume(1);
            return b;
        }

        public int ReadChar()
        {
            EnsureOpen();
            int started = Environment.TickCount;
            Fill(0);
            int length = CharLength(_readPos);
            while (length == 0)
            {
                WaitForMore(started);
                length = CharLength(_readPos);
            }
            string c = Decode(_readPos, length);
            if (c.Length > 1) throw new ArgumentException("The output char buffer is too small to contain the decoded characters.", "chars");
            Consume(length);
            return c[0];
        }

        public int Read(char[] buffer, int offset, int count)
        {
            EnsureOpen();
            if ((object)buffer == null) throw new ArgumentNullException("buffer");
            ValidateRange(buffer.Length, offset, count);
            if (count == 0) return 0;
            int started = Environment.TickCount;
            Fill(0);
            while (CharLength(_readPos) == 0) WaitForMore(started);
            int written = 0;
            while (written < count)
            {
                int length = CharLength(_readPos);
                if (length == 0) break;
                string c = Decode(_readPos, length);
                if (written + c.Length > count) break;
                for (int i = 0; i < c.Length; i++) buffer[offset + written + i] = c[i];
                written = written + c.Length;
                Consume(length);
            }
            return written;
        }

        public string ReadExisting()
        {
            EnsureOpen();
            Fill(0);
            int whole = 0;
            int length = CharLength(_readPos);
            while (length > 0)
            {
                whole = whole + length;
                length = CharLength(_readPos + whole);
            }
            if (whole == 0) return "";
            string text = Decode(_readPos, whole);
            Consume(whole);
            return text;
        }

        public string ReadTo(string value)
        {
            EnsureOpen();
            if ((object)value == null) throw new ArgumentNullException("value");
            if (value.Length == 0) throw new ArgumentException("Argument value cannot be null or zero-length.", "value");
            int started = Environment.TickCount;
            Fill(0);
            string text = "";
            int scanned = 0;
            while (true)
            {
                int length = CharLength(_readPos + scanned);
                if (length == 0)
                {
                    WaitForMore(started);
                    continue;
                }
                text = String.Concat(text, Decode(_readPos + scanned, length));
                scanned = scanned + length;
                if (text.Length >= value.Length && text.EndsWith(value))
                {
                    Consume(scanned);
                    return text.Substring(0, text.Length - value.Length);
                }
            }
        }

        public string ReadLine()
        {
            return ReadTo(_newLine);
        }

        private void Consume(int count)
        {
            _readPos = _readPos + count;
            if (_readPos == _readLen)
            {
                _readPos = 0;
                _readLen = 0;
            }
        }

        private int Fill(int timeoutMs)
        {
            int waiting = NativeSerial.BytesToRead(_handle);
            if (waiting < 0) NativeSerial.Throw(waiting, _portName);
            if (waiting == 0 && timeoutMs == 0) return 0;
            int want = waiting > 0 ? waiting : 1;
            if (_inBuffer == null) _inBuffer = new byte[want > 64 ? want : 64];
            if (_readPos > 0)
            {
                int kept = _readLen - _readPos;
                for (int i = 0; i < kept; i++) _inBuffer[i] = _inBuffer[_readPos + i];
                _readPos = 0;
                _readLen = kept;
            }
            if (_inBuffer.Length - _readLen < want)
            {
                int size = _inBuffer.Length * 2;
                while (size - _readLen < want) size = size * 2;
                byte[] bigger = new byte[size];
                for (int i = 0; i < _readLen; i++) bigger[i] = _inBuffer[i];
                _inBuffer = bigger;
            }
            int n = NativeSerial.Read(_handle, _inBuffer, _readLen, want, waiting > 0 ? 0 : timeoutMs);
            if (n < 0) NativeSerial.Throw(n, _portName);
            _readLen = _readLen + n;
            return n;
        }

        private void WaitForMore(int started)
        {
            int wait = InfiniteTimeout;
            if (_readTimeout != InfiniteTimeout)
            {
                wait = _readTimeout - (Environment.TickCount - started);
                if (wait < 0) throw new TimeoutException();
            }
            if (Fill(wait) == 0) throw new TimeoutException();
        }

        private int CharLength(int pos)
        {
            int available = _readLen - pos;
            if (available <= 0) return 0;
            if (_encoding is System.Text.UTF8Encoding)
            {
                int lead = _inBuffer[pos];
                int length = 1;
                if ((lead & 0xE0) == 0xC0) length = 2;
                else if ((lead & 0xF0) == 0xE0) length = 3;
                else if ((lead & 0xF8) == 0xF0) length = 4;
                for (int i = 1; i < length; i++)
                {
                    if (i >= available) return 0;
                    if ((_inBuffer[pos + i] & 0xC0) != 0x80) return i;
                }
                return length;
            }
            if (_encoding is System.Text.UnicodeEncoding)
            {
                if (available < 2) return 0;
                int unit = _inBuffer[pos] | (_inBuffer[pos + 1] << 8);
                if (unit < 0xD800 || unit > 0xDBFF) return 2;
                if (available < 4) return 0;
                int next = _inBuffer[pos + 2] | (_inBuffer[pos + 3] << 8);
                return (next >= 0xDC00 && next <= 0xDFFF) ? 4 : 2;
            }
            return 1;
        }

        private string Decode(int pos, int length)
        {
            byte[] bytes = new byte[length];
            for (int i = 0; i < length; i++) bytes[i] = _inBuffer[pos + i];
            return _encoding.GetString(bytes);
        }

        public void Close()
        {
            Dispose(true);
        }

        public void Dispose()
        {
            Dispose(true);
        }

        protected virtual void Dispose(bool disposing)
        {
            if (_handle >= 0)
            {
                NativeSerial.Close(_handle);
                _handle = -1;
            }
            _readPos = 0;
            _readLen = 0;
            if (_baseStream != null)
            {
                _baseStream.Dispose();
                _baseStream = null;
            }
        }

        private void EnsureOpen()
        {
            if (_handle < 0) throw new InvalidOperationException("The port is closed.");
        }

        private void EnsureNotOpen()
        {
            if (_handle >= 0) throw new InvalidOperationException("The port setting cannot be changed while the port is open.");
        }

        private static void ValidateRange(int length, int offset, int count)
        {
            if (offset < 0) throw new ArgumentOutOfRangeException("offset", "Non-negative number required.");
            if (count < 0) throw new ArgumentOutOfRangeException("count", "Non-negative number required.");
            if (length - offset < count)
                throw new ArgumentException("Offset and length were out of bounds for the array or count is greater than the number of elements from index to the end of the source collection.");
        }

        private static void ValidateRange(byte[] buffer, int offset, int count)
        {
            if ((object)buffer == null) throw new ArgumentNullException("buffer");
            ValidateRange(buffer.Length, offset, count);
        }
    }
}
#endif
