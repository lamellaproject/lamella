// Lamella.Hardware -- the bus table an image built for a board carries, in the System.Device.Gpio assembly.
namespace Lamella.Hardware
{
    /// <summary>The bus table of the board an image was built for: for each bus, the words its
    /// family's driver is constructed from.</summary>
    internal sealed class BoardBuses
    {
        private BoardBuses() { }

        /// <summary>The kind of the board's GPIO controller, which is always bus 0.</summary>
        internal const int Gpio = 1;

        /// <summary>The kind of an I2C bus.</summary>
        internal const int I2c = 2;

        /// <summary>The kind of an SPI bus.</summary>
        internal const int Spi = 3;

        /// <summary>How many words bus <paramref name="bus"/> of <paramref name="kind"/> has: -1
        /// when the image carries no table, 0 when the board has no such bus.</summary>
        [Lamella.Runtime.RuntimeProvided]
        internal static int Length(int kind, int bus) { return -1; }

        /// <summary>Word <paramref name="index"/> of that bus's row, for 0 &lt;= index &lt;
        /// <see cref="Length"/>; 0 outside it.</summary>
        [Lamella.Runtime.RuntimeProvided]
        internal static uint Word(int kind, int bus, int index) { return 0; }
    }
}
