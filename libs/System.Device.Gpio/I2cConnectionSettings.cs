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

        internal I2cConnectionSettings Clone()
        {
            return new I2cConnectionSettings(_busId, _deviceAddress);
        }
    }
}
