// Lamella System.Device.I2c -- the dotnet/iot I2C API, in the System.Device.Gpio assembly.
using Lamella.Hardware;

namespace System.Device.I2c
{
    /// <summary>The communications channel to a device on an I2C bus.</summary>
    public abstract class I2cDevice : System.IDisposable
    {
        /// <summary>Initializes the base class.</summary>
        protected I2cDevice()
        {
        }

        /// <summary>The connection settings of the device. The settings are immutable after
        /// the device is created, so the returned object is a clone.</summary>
        public abstract I2cConnectionSettings ConnectionSettings { get; }

        /// <summary>Reads data from the device, filling <paramref name="buffer"/> in one
        /// transaction.</summary>
        public abstract void Read(System.Span<byte> buffer);

        /// <summary>Reads a byte from the device.</summary>
        public virtual byte ReadByte()
        {
            byte[] buffer = new byte[1];
            Read(new System.Span<byte>(buffer));
            return buffer[0];
        }

        /// <summary>Writes <paramref name="buffer"/> to the device in one transaction.</summary>
        public abstract void Write(System.ReadOnlySpan<byte> buffer);

        /// <summary>Writes a byte to the device.</summary>
        public virtual void WriteByte(byte value)
        {
            byte[] buffer = new byte[1];
            buffer[0] = value;
            Write(new System.ReadOnlySpan<byte>(buffer));
        }

        /// <summary>Performs an atomic write-then-read: the write bytes go out, a RESTART
        /// condition (not a STOP) follows, and the read fills <paramref name="readBuffer"/>
        /// -- the register-read idiom.</summary>
        public abstract void WriteRead(System.ReadOnlySpan<byte> writeBuffer, System.Span<byte> readBuffer);

        /// <summary>Creates a communications channel to the device described by
        /// <paramref name="settings"/>, over the driver the board bound for
        /// <see cref="I2cConnectionSettings.BusId"/>. The driver is shared by every device on
        /// the bus, so disposing this device does not dispose it -- the same sharing a device
        /// created through <see cref="I2cBus.CreateDevice"/> already has.</summary>
        /// <exception cref="System.InvalidOperationException">No driver is bound for the
        /// settings' bus.</exception>
        public static I2cDevice Create(I2cConnectionSettings settings)
        {
            if ((object)settings == null) throw new System.ArgumentNullException("settings");
            return new DriverI2cDevice(settings.Clone(), Buses.ResolveI2c(settings.BusId), false);
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

    internal sealed class DriverI2cDevice : I2cDevice
    {
        private readonly I2cConnectionSettings _settings;
        private readonly I2cDriver _driver;
        private readonly bool _ownsDriver;
        private readonly byte[] _one;

        internal DriverI2cDevice(I2cConnectionSettings settings, I2cDriver driver, bool ownsDriver)
        {
            _settings = settings;
            _driver = driver;
            _ownsDriver = ownsDriver;
            _one = new byte[1];
        }

        public override I2cConnectionSettings ConnectionSettings
        {
            get { return _settings.Clone(); }
        }

        public override void Read(System.Span<byte> buffer)
        {
            Check(_driver.Read(_settings.DeviceAddress, buffer, buffer.Length), false);
        }

        public override byte ReadByte()
        {
            Check(_driver.Read(_settings.DeviceAddress, new System.Span<byte>(_one), 1), false);
            return _one[0];
        }

        public override void Write(System.ReadOnlySpan<byte> buffer)
        {
            Check(_driver.Write(_settings.DeviceAddress, buffer, buffer.Length), true);
        }

        public override void WriteByte(byte value)
        {
            _one[0] = value;
            Check(_driver.Write(_settings.DeviceAddress, new System.ReadOnlySpan<byte>(_one), 1), true);
        }

        public override void WriteRead(System.ReadOnlySpan<byte> writeBuffer, System.Span<byte> readBuffer)
        {
            Check(_driver.WriteRead(_settings.DeviceAddress, writeBuffer, writeBuffer.Length,
                readBuffer, readBuffer.Length), true);
        }

        private void Check(int status, bool writing)
        {
            if (status == I2cDriver.Ok) return;
            string address = "0x" + _settings.DeviceAddress.ToString("X2");
            if (status == I2cDriver.AddressNack)
            {
                throw new System.IO.IOException("no acknowledgment from address " + address);
            }
            if (status == I2cDriver.DataNack)
            {
                throw new System.IO.IOException(writing
                    ? "no acknowledgment while writing to " + address
                    : "no acknowledgment while reading from " + address);
            }
            throw new System.IO.IOException(
                "I2C transfer failed for address " + address + " on bus " + _settings.BusId +
                " (status " + status + ").");
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
