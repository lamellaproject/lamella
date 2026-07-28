// Lamella.Hardware -- the explicit driver-taking constructors, in the System.Device.Gpio assembly.
using System.Device.I2c;
using System.Device.Spi;

namespace Lamella.Hardware
{
    /// <summary>Creates a device or bus over a driver supplied directly, for the cases that do not
    /// go through the board's <see cref="Buses"/> table.</summary>
    public sealed class BusFactory
    {
        private BusFactory() { }

        /// <summary>Creates a SPI communications channel over <paramref name="driver"/>,
        /// configuring it with a private copy of <paramref name="settings"/>. The returned device
        /// owns the driver and disposes it.</summary>
        public static SpiDevice CreateSpiDevice(SpiConnectionSettings settings, SpiDriver driver)
        {
            if ((object)settings == null) throw new System.ArgumentNullException("settings");
            if ((object)driver == null) throw new System.ArgumentNullException("driver");
            return new DriverSpiDevice(settings.Clone(), driver, true);
        }

        /// <summary>Creates an I2C communications channel over <paramref name="driver"/>. The
        /// returned device owns the driver and disposes it.</summary>
        public static I2cDevice CreateI2cDevice(I2cConnectionSettings settings, I2cDriver driver)
        {
            if ((object)settings == null) throw new System.ArgumentNullException("settings");
            if ((object)driver == null) throw new System.ArgumentNullException("driver");
            return new DriverI2cDevice(settings.Clone(), driver, true);
        }

        /// <summary>Creates an I2C bus channel for <paramref name="busId"/> over
        /// <paramref name="driver"/>. Devices created from the bus share it.</summary>
        public static I2cBus CreateI2cBus(int busId, I2cDriver driver)
        {
            if ((object)driver == null) throw new System.ArgumentNullException("driver");
            return new DriverI2cBus(busId, driver);
        }
    }
}
