// System.IO.Ports (libs/, real .NET's own assembly name) -- System.IO.Ports.SerialPort
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

        public int BytesToRead
        {
            get
            {
                EnsureOpen();
                int n = NativeSerial.BytesToRead(_handle);
                if (n < 0) NativeSerial.Throw(n, _portName);
                return n;
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
            ValidateRange(buffer, offset, count);
            EnsureOpen();
            int read = NativeSerial.Read(_handle, buffer, offset, count, _readTimeout);
            if (read < 0) NativeSerial.Throw(read, _portName);
            return read;
        }

        public void Write(byte[] buffer, int offset, int count)
        {
            ValidateRange(buffer, offset, count);
            EnsureOpen();
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
        }

        public void DiscardOutBuffer()
        {
            EnsureOpen();
            int code = NativeSerial.DiscardOut(_handle);
            if (code < 0) NativeSerial.Throw(code, _portName);
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
