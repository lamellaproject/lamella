// System.Device.Spi -- the dotnet/iot SPI API, shipped in the System.Device.Gpio assembly (Microsoft's official packaging of the Spi/I2c namespaces).
namespace System.Device.Spi
{
    /// <summary>Defines how data is synchronized between devices on a SPI bus:
    /// the clock polarity (CPOL, idle level) and phase (CPHA, sampling edge).</summary>
    public enum SpiMode
    {
        /// <summary>CPOL 0, CPHA 0: clock idles low, data sampled on the rising edge.</summary>
        Mode0 = 0,
        /// <summary>CPOL 0, CPHA 1: clock idles low, data sampled on the falling edge.</summary>
        Mode1 = 1,
        /// <summary>CPOL 1, CPHA 0: clock idles high, data sampled on the falling edge.</summary>
        Mode2 = 2,
        /// <summary>CPOL 1, CPHA 1: clock idles high, data sampled on the rising edge.</summary>
        Mode3 = 3
    }
}
