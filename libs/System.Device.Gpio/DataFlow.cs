// Lamella System.Device.Spi -- the dotnet/iot SPI API, in the System.Device.Gpio assembly.
namespace System.Device.Spi
{
    /// <summary>Specifies the order in which bits are transferred on the SPI bus.</summary>
    public enum DataFlow
    {
        /// <summary>The most significant bit is transferred first.</summary>
        MsbFirst = 0,
        /// <summary>The least significant bit is transferred first.</summary>
        LsbFirst = 1
    }
}
