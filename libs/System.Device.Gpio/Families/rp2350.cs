// Lamella.Hardware -- the RP2350's drivers, constructed from the board's bus table, in the rp2350 build of System.Device.Gpio.
using Lamella.Boards;
using System.Device.Gpio;

namespace Lamella.Hardware
{
    /// <summary>Constructs the RP2350 drivers a board's bus table names.</summary>
    internal sealed class BoardDrivers
    {
        private BoardDrivers() { }

        /// <summary>The family whose drivers this build carries.</summary>
        internal const string Family = "rp2350";

        /// <summary>I2C bus <paramref name="bus"/>'s driver, or null when the image carries no
        /// table or the board has no such bus.</summary>
        internal static I2cDriver CreateI2c(int bus)
        {
            int length = BoardBuses.Length(BoardBuses.I2c, bus);
            if (length <= 0) return null;
            Expect(BoardBuses.I2c, bus, length, 8);
            return new Rp2350I2cDriver(new Rp2350I2cBinding(
                Word(BoardBuses.I2c, bus, 0), Word(BoardBuses.I2c, bus, 1),
                Word(BoardBuses.I2c, bus, 2), Word(BoardBuses.I2c, bus, 3),
                Word(BoardBuses.I2c, bus, 4), Word(BoardBuses.I2c, bus, 5),
                Word(BoardBuses.I2c, bus, 6), Word(BoardBuses.I2c, bus, 7)));
        }

        /// <summary>SPI bus <paramref name="bus"/>'s driver, or null when the image carries no
        /// table or the board has no such bus.</summary>
        internal static SpiDriver CreateSpi(int bus)
        {
            int length = BoardBuses.Length(BoardBuses.Spi, bus);
            if (length <= 0) return null;
            int selects = length > 10 ? (int)Word(BoardBuses.Spi, bus, 10) : 0;
            Expect(BoardBuses.Spi, bus, length, 11 + selects);
            int[] pins = new int[selects];
            for (int select = 0; select < selects; select++)
            {
                pins[select] = (int)Word(BoardBuses.Spi, bus, 11 + select);
            }
            return new Rp2350SpiDriver(new Rp2350SpiBinding(
                Word(BoardBuses.Spi, bus, 0), Word(BoardBuses.Spi, bus, 1),
                Word(BoardBuses.Spi, bus, 2), Word(BoardBuses.Spi, bus, 3),
                Word(BoardBuses.Spi, bus, 4), Word(BoardBuses.Spi, bus, 5),
                Word(BoardBuses.Spi, bus, 6), Word(BoardBuses.Spi, bus, 7),
                Word(BoardBuses.Spi, bus, 8), Word(BoardBuses.Spi, bus, 9), pins));
        }

        /// <summary>Whether this build constructs a driver for bus <paramref name="bus"/> of
        /// <paramref name="kind"/>: whether the image carries a table and it names that bus.</summary>
        internal static bool Serves(int kind, int bus)
        {
            return BoardBuses.Length(kind, bus) > 0;
        }

        /// <summary>The GPIO controller's driver, or null when the image carries no table or the
        /// board has no controller.</summary>
        internal static GpioDriver CreateGpio()
        {
            int length = BoardBuses.Length(BoardBuses.Gpio, 0);
            if (length <= 0) return null;
            int pairs = length > 1 ? (int)Word(BoardBuses.Gpio, 0, 1) : 0;
            Expect(BoardBuses.Gpio, 0, length, 2 + 2 * pairs);
            int[] paired = new int[pairs];
            int[] partners = new int[pairs];
            for (int pair = 0; pair < pairs; pair++)
            {
                paired[pair] = (int)Word(BoardBuses.Gpio, 0, 2 + 2 * pair);
                partners[pair] = (int)Word(BoardBuses.Gpio, 0, 3 + 2 * pair);
            }
            return new Rp2350GpioDriver(Word(BoardBuses.Gpio, 0, 0), paired, partners);
        }

        /// <summary>Why bus <paramref name="bus"/> of <paramref name="kind"/> has no driver, as the
        /// image's bus table says.</summary>
        internal static string Describe(int kind, int bus)
        {
            return BoardTableReport.Describe(kind, bus);
        }

        private static uint Word(int kind, int bus, int index)
        {
            return BoardBuses.Word(kind, bus, index);
        }

        private static void Expect(int kind, int bus, int length, int expected)
        {
            if (length == expected) return;
            throw new System.IO.IOException(
                "the image's bus table gives kind " + kind.ToString() + " bus " + bus.ToString() + " "
                + length.ToString() + " words, and this build's " + Family + " driver for it takes "
                + expected.ToString());
        }
    }
}
