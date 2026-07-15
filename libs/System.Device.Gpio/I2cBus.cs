// Lamella System.Device.I2c -- the dotnet/iot I2C API, in the System.Device.Gpio assembly.
namespace System.Device.I2c
{
    /// <summary>An I2C bus communication channel that hands out per-device channels.</summary>
    public abstract class I2cBus : System.IDisposable
    {
        /// <summary>Initializes the base class.</summary>
        protected I2cBus()
        {
        }

        /// <summary>Creates a communication channel to the device at 7-bit
        /// <paramref name="deviceAddress"/> on this bus.</summary>
        public abstract I2cDevice CreateDevice(int deviceAddress);

        /// <summary>Removes the device at <paramref name="deviceAddress"/> from this bus,
        /// releasing the address for a later <see cref="CreateDevice"/>.</summary>
        public abstract void RemoveDevice(int deviceAddress);

        /// <summary>Creates a bus channel for <paramref name="busId"/> over
        /// <paramref name="driver"/> (the explicit chip binding this tier uses in place of
        /// a platform registry). The bus owns the driver; devices created from the bus
        /// share it.</summary>
        public static I2cBus Create(int busId, I2cDriver driver)
        {
            if ((object)driver == null) throw new System.ArgumentNullException("driver");
            return new DriverI2cBus(busId, driver);
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

    internal sealed class DriverI2cBus : I2cBus
    {
        private readonly int _busId;
        private readonly I2cDriver _driver;
        private readonly bool[] _claimed;

        internal DriverI2cBus(int busId, I2cDriver driver)
        {
            _busId = busId;
            _driver = driver;
            _claimed = new bool[128];
        }

        public override I2cDevice CreateDevice(int deviceAddress)
        {
            if (deviceAddress < 0 || deviceAddress > 127)
            {
                throw new System.ArgumentOutOfRangeException("deviceAddress");
            }
            if (_claimed[deviceAddress])
            {
                throw new System.InvalidOperationException(
                    "address 0x" + deviceAddress.ToString("X2") + " already has a device on bus " + _busId);
            }
            _claimed[deviceAddress] = true;
            return new DriverI2cDevice(new I2cConnectionSettings(_busId, deviceAddress), _driver, false);
        }

        public override void RemoveDevice(int deviceAddress)
        {
            if (deviceAddress < 0 || deviceAddress > 127 || !_claimed[deviceAddress])
            {
                throw new System.ArgumentException(
                    "no device at address " + deviceAddress + " on bus " + _busId, "deviceAddress");
            }
            _claimed[deviceAddress] = false;
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
