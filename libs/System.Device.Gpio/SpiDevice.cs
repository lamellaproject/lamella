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
        /// <see cref="SpiConnectionSettings.BusId"/>. Configures that driver with a private copy
        /// of the settings. The driver is shared by every device on the bus, so disposing this
        /// device does not dispose it.</summary>
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
            driver.Configure(settings);
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
            if (writeBuffer.Length != readBuffer.Length)
            {
                throw new System.ArgumentException("The write and read buffers must be the same length.");
            }
            Transfer(writeBuffer, readBuffer, writeBuffer.Length);
        }

        private void Transfer(System.ReadOnlySpan<byte> writeBuffer, System.Span<byte> readBuffer, int count)
        {
            _driver.SetChipSelect(true);
            int status;
            try
            {
                status = _driver.TransferFullDuplex(writeBuffer, readBuffer, count);
            }
            finally
            {
                _driver.SetChipSelect(false);
            }
            if (status != 0)
            {
                throw new System.IO.IOException(
                    "SPI transfer failed on bus " + _settings.BusId + " (status " + status + ").");
            }
        }

        protected override void Dispose(bool disposing)
        {
            if (disposing && _ownsDriver)
            {
                _driver.Dispose();
            }
        }
    }
}
