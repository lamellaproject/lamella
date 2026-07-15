// Lamella System.Device.Spi -- the dotnet/iot SPI API, in the System.Device.Gpio assembly.
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
        public abstract void Read(byte[] buffer);

        /// <summary>Reads a byte from the SPI device.</summary>
        public abstract byte ReadByte();

        /// <summary>Writes <paramref name="buffer"/> to the SPI device.</summary>
        public abstract void Write(byte[] buffer);

        /// <summary>Writes a byte to the SPI device.</summary>
        public abstract void WriteByte(byte value);

        /// <summary>Writes and reads data as one full-duplex operation: every written word
        /// clocks a word in. The buffers must be the same length.</summary>
        public abstract void TransferFullDuplex(byte[] writeBuffer, byte[] readBuffer);

        /// <summary>Creates a communications channel to the device described by
        /// <paramref name="settings"/> over <paramref name="driver"/> (the explicit chip
        /// binding this tier uses in place of a platform registry). Configures the driver
        /// with a private copy of the settings; the device owns the driver.</summary>
        public static SpiDevice Create(SpiConnectionSettings settings, SpiDriver driver)
        {
            if ((object)settings == null) throw new System.ArgumentNullException("settings");
            if ((object)driver == null) throw new System.ArgumentNullException("driver");
            return new DriverSpiDevice(settings.Clone(), driver);
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
        private readonly byte[] _oneOut;
        private readonly byte[] _oneIn;

        internal DriverSpiDevice(SpiConnectionSettings settings, SpiDriver driver)
        {
            _settings = settings;
            _driver = driver;
            _oneOut = new byte[1];
            _oneIn = new byte[1];
            driver.Configure(settings);
        }

        public override SpiConnectionSettings ConnectionSettings
        {
            get { return _settings.Clone(); }
        }

        public override void Read(byte[] buffer)
        {
            if ((object)buffer == null) throw new System.ArgumentNullException("buffer");
            Transfer(null, buffer, buffer.Length);
        }

        public override byte ReadByte()
        {
            Transfer(null, _oneIn, 1);
            return _oneIn[0];
        }

        public override void Write(byte[] buffer)
        {
            if ((object)buffer == null) throw new System.ArgumentNullException("buffer");
            Transfer(buffer, null, buffer.Length);
        }

        public override void WriteByte(byte value)
        {
            _oneOut[0] = value;
            Transfer(_oneOut, null, 1);
        }

        public override void TransferFullDuplex(byte[] writeBuffer, byte[] readBuffer)
        {
            if ((object)writeBuffer == null) throw new System.ArgumentNullException("writeBuffer");
            if ((object)readBuffer == null) throw new System.ArgumentNullException("readBuffer");
            if (writeBuffer.Length != readBuffer.Length)
            {
                throw new System.ArgumentException("The write and read buffers must be the same length.");
            }
            Transfer(writeBuffer, readBuffer, writeBuffer.Length);
        }

        private void Transfer(byte[] writeBuffer, byte[] readBuffer, int count)
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
            if (disposing)
            {
                _driver.Dispose();
            }
        }
    }
}
