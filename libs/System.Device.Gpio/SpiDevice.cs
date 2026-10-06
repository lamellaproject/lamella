// Lamella System.Device.Spi -- the dotnet/iot SPI API, in the System.Device.Gpio assembly.
using Lamella.Hardware;

namespace System.Device.Spi
{
    /// <summary>The communications channel to a device on a SPI bus.</summary>
    public abstract class SpiDevice : System.IDisposable
    {
        /// <summary>Initializes the base class.</summary>
        protected SpiDevice()
        {
        }

        /// <summary>The connection settings of the device. The settings are immutable after
        /// the device is created, so the returned object is a clone.</summary>
        public abstract SpiConnectionSettings ConnectionSettings { get; }

        /// <summary>Reads data from the SPI device, filling <paramref name="buffer"/>.</summary>
        public abstract void Read(System.Span<byte> buffer);

        /// <summary>Reads a byte from the SPI device.</summary>
        public virtual byte ReadByte()
        {
            byte[] buffer = new byte[1];
            Read(new System.Span<byte>(buffer));
            return buffer[0];
        }

        /// <summary>Writes <paramref name="buffer"/> to the SPI device.</summary>
        public abstract void Write(System.ReadOnlySpan<byte> buffer);

        /// <summary>Writes a byte to the SPI device.</summary>
        public virtual void WriteByte(byte value)
        {
            byte[] buffer = new byte[1];
            buffer[0] = value;
            Write(new System.ReadOnlySpan<byte>(buffer));
        }

        /// <summary>Writes and reads data as one full-duplex operation: every written word
        /// clocks a word in. The buffers must be the same length.</summary>
        public abstract void TransferFullDuplex(System.ReadOnlySpan<byte> writeBuffer, System.Span<byte> readBuffer);

        /// <summary>Creates a communications channel to the device described by
        /// <paramref name="settings"/>, over the driver the board bound for
        /// <see cref="SpiConnectionSettings.BusId"/>, with a private copy of the settings. The
        /// driver is shared by every device on the bus, so disposing this device does not dispose
        /// it.</summary>
        /// <remarks>
        /// <para>Each device keeps its own chip select, mode and clock: before a transfer that follows
        /// another device's, the device applies its own settings to the shared driver, and it holds
        /// the bus from then until its chip select is released, so two threads' transfers never
        /// interleave. Creating the device applies its settings once, which drives its chip select
        /// idle before any other device's transfer.</para>
        /// <para>A <see cref="SpiConnectionSettings.ChipSelectLine"/> that is neither -1 nor one of
        /// the bus's chip selects is refused at the device's first transfer with an
        /// <see cref="System.IO.IOException"/> that names the bus's chip selects, as dotnet/iot's
        /// Linux device refuses it when it opens <c>/dev/spidev</c><i>B</i>.<i>C</i> at its first
        /// transfer.</para>
        /// </remarks>
        /// <exception cref="System.InvalidOperationException">No driver is bound for the
        /// settings' bus.</exception>
        public static SpiDevice Create(SpiConnectionSettings settings)
        {
            if ((object)settings == null) throw new System.ArgumentNullException("settings");
            return new DriverSpiDevice(settings.Clone(), Buses.ResolveSpi(settings.BusId), false);
        }

        /// <summary>Disposes this instance.</summary>
        public void Dispose()
        {
            Dispose(true);
        }

        /// <summary>Disposes this instance.</summary>
        protected virtual void Dispose(bool disposing)
        {
        }
    }

    internal sealed class DriverSpiDevice : SpiDevice
    {
        private readonly SpiConnectionSettings _settings;
        private readonly SpiDriver _driver;
        private readonly bool _ownsDriver;
        private readonly bool _selectable;
        private readonly byte[] _oneOut;
        private readonly byte[] _oneIn;
        private readonly byte[] _none;

        internal DriverSpiDevice(SpiConnectionSettings settings, SpiDriver driver, bool ownsDriver)
        {
            _settings = settings;
            _driver = driver;
            _ownsDriver = ownsDriver;
            _oneOut = new byte[1];
            _oneIn = new byte[1];
            _none = new byte[0];
            int line = settings.ChipSelectLine;
            _selectable = line == -1 || (line >= 0 && line < driver.ChipSelectCount);
            if (_selectable)
            {
                EnterBus();
                try
                {
                    Apply();
                }
                finally
                {
                    ExitBus();
                }
            }
        }

        public override SpiConnectionSettings ConnectionSettings
        {
            get { return _settings.Clone(); }
        }

        internal uint NativeBusIdentity
        {
            get { return _driver.NativeBusIdentity; }
        }

        public override void Read(System.Span<byte> buffer)
        {
            Transfer(new System.ReadOnlySpan<byte>(_none), buffer, buffer.Length);
        }

        public override byte ReadByte()
        {
            Transfer(new System.ReadOnlySpan<byte>(_none), new System.Span<byte>(_oneIn), 1);
            return _oneIn[0];
        }

        public override void Write(System.ReadOnlySpan<byte> buffer)
        {
            Transfer(buffer, new System.Span<byte>(_none), buffer.Length);
        }

        public override void WriteByte(byte value)
        {
            _oneOut[0] = value;
            Transfer(new System.ReadOnlySpan<byte>(_oneOut), new System.Span<byte>(_none), 1);
        }

        public override void TransferFullDuplex(System.ReadOnlySpan<byte> writeBuffer, System.Span<byte> readBuffer)
        {
            RefuseUnlessSelectable();
            if (writeBuffer.Length != readBuffer.Length)
            {
                throw new System.ArgumentException("The write and read buffers must be the same length.");
            }
            Transfer(writeBuffer, readBuffer, writeBuffer.Length);
        }

        private void Transfer(System.ReadOnlySpan<byte> writeBuffer, System.Span<byte> readBuffer, int count)
        {
            RefuseUnlessSelectable();
            int status;
            EnterBus();
            try
            {
                Apply();
                _driver.SetChipSelect(true);
                try
                {
                    status = _driver.TransferFullDuplex(writeBuffer, readBuffer, count);
                }
                finally
                {
                    _driver.SetChipSelect(false);
                }
            }
            finally
            {
                ExitBus();
            }
            if (status != 0)
            {
                throw new System.IO.IOException(
                    "SPI transfer failed on bus " + _settings.BusId + " (status " + status + ").");
            }
        }

        private void Apply()
        {
            if ((object)_driver.ConfiguredFor == (object)this) return;
            _driver.ConfiguredFor = null;
            _driver.Configure(_settings);
            _driver.ConfiguredFor = this;
        }

        private void RefuseUnlessSelectable()
        {
            if (_selectable) return;
            throw new System.IO.IOException(NotAChipSelect());
        }

        private string NotAChipSelect()
        {
            string text = "ChipSelectLine " + _settings.ChipSelectLine.ToString()
                + " is not a chip select of SPI bus " + _settings.BusId.ToString();
            int count = _driver.ChipSelectCount;
            if (count <= 0)
            {
                return text + ", which has none: -1 is the only line";
            }
            text = text + ":";
            for (int line = 0; line < count; line++)
            {
                text = text + " line " + line.ToString() + " is GPIO " + _driver.GetChipSelectPin(line).ToString() + ",";
            }
            return text + " and -1 is none";
        }

        private void EnterBus()
        {
#if LAMELLA_SURFACE_THREADS
            System.Threading.Monitor.Enter(_driver);
#endif
        }

        private void ExitBus()
        {
#if LAMELLA_SURFACE_THREADS
            System.Threading.Monitor.Exit(_driver);
#endif
        }

        protected override void Dispose(bool disposing)
        {
            if (!disposing) return;
            if (_ownsDriver)
            {
                _driver.Dispose();
                return;
            }
            EnterBus();
            try
            {
                if ((object)_driver.ConfiguredFor == (object)this) _driver.ConfiguredFor = null;
            }
            finally
            {
                ExitBus();
            }
        }
    }
}
