// Lamella.Hardware.SpiNative -- naming an open SPI bus to a native owner, in the
namespace Lamella.Hardware
{
    /// <summary>Names the peripheral instance behind an open SPI device, for a native owner that
    /// takes the bus over.</summary>
    public sealed class SpiNative
    {
        private SpiNative() { }

        /// <summary>The value by which a native owner names the peripheral instance
        /// <paramref name="device"/> drives, or 0 when there is none to name -- a device over a
        /// bit-banged or host-backed driver, or one not built through
        /// <see cref="System.Device.Spi.SpiDevice.Create"/>. On a memory-mapped chip the value is
        /// the peripheral's register base; it is an IDENTIFIER to match on, never an address this
        /// tier reads or writes.</summary>
        public static uint IdentityOf(System.Device.Spi.SpiDevice device)
        {
            System.Device.Spi.DriverSpiDevice bound = device as System.Device.Spi.DriverSpiDevice;
            if ((object)bound == null)
                return 0;
            return bound.NativeBusIdentity;
        }
    }
}
