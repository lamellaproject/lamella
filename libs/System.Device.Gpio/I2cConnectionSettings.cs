// System.Device.I2c -- the dotnet/iot I2C API, shipped in the System.Device.Gpio assembly (Microsoft's official packaging of the Spi/I2c namespaces).
namespace System.Device.I2c
{
    /// <summary>The connection settings of a device on an I2C bus.</summary>
    public sealed class I2cConnectionSettings
    {
        private readonly int _busId;
        private readonly int _deviceAddress;

        public I2cConnectionSettings(int busId, int deviceAddress)
        {
            _busId = busId;
            _deviceAddress = deviceAddress;
        }

        public int BusId { get { return _busId; } }

        /// <summary>The device's 7-bit bus address.</summary>
        public int DeviceAddress { get { return _deviceAddress; } }

        /// <summary>Whether these settings name the same bus and device address as another.</summary>
        /// <param name="other">The settings to compare with, which may be null.</param>
        public bool Equals(I2cConnectionSettings other)
        {
            if ((object)other == null)
            {
                return false;
            }
            if ((object)this == (object)other)
            {
                return true;
            }
            return _busId == other._busId && _deviceAddress == other._deviceAddress;
        }

        /// <summary>Whether this object equals another.</summary>
        /// <param name="obj">The object to compare with.</param>
        public override bool Equals(object obj)
        {
            return Equals(obj as I2cConnectionSettings);
        }

        /// <summary>A hash code combining the bus id and the device address.</summary>
        public override int GetHashCode()
        {
            unchecked
            {
                return (_busId * 397) ^ _deviceAddress;
            }
        }

        internal I2cConnectionSettings Clone()
        {
            return new I2cConnectionSettings(_busId, _deviceAddress);
        }
    }
}
