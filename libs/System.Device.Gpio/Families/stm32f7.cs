// Lamella.Hardware -- the STM32F746's drivers, constructed from the board's bus table, in the stm32f7 build of System.Device.Gpio.
using System.Device.Gpio;

namespace Lamella.Hardware
{
    /// <summary>Constructs the STM32F746 drivers a board's bus table names.</summary>
    internal sealed class BoardDrivers
    {
        private BoardDrivers() { }

        /// <summary>The family whose drivers this build carries.</summary>
        internal const string Family = "stm32f7";

        /// <summary>I2C bus <paramref name="bus"/>'s driver: null, as this build carries no I2C
        /// driver.</summary>
        internal static I2cDriver CreateI2c(int bus) { return null; }

        /// <summary>SPI bus <paramref name="bus"/>'s driver: null, as this build carries no SPI
        /// driver.</summary>
        internal static SpiDriver CreateSpi(int bus) { return null; }

        /// <summary>Whether this build constructs a driver for bus <paramref name="bus"/> of
        /// <paramref name="kind"/>: the GPIO controller, where the image carries a table naming it.</summary>
        internal static bool Serves(int kind, int bus)
        {
            return kind == BoardBuses.Gpio && BoardBuses.Length(kind, bus) > 0;
        }

        /// <summary>The GPIO controller's driver, or null when the image carries no table or the board
        /// has no controller.</summary>
        internal static GpioDriver CreateGpio()
        {
            int length = BoardBuses.Length(BoardBuses.Gpio, 0);
            if (length <= 0) return null;
            Stm32GpioPorts ports = Stm32f7GpioBinding.Ports();
            int count = ports.Bases.Length;
            if (length != 2 * count) Expect(BoardBuses.Gpio, 0, length, count);
            return new Stm32GpioDriver(ports, Stm32f7GpioBinding.Lines(), Words(0, count),
                length == count ? null : Words(count, count));
        }

        private static uint[] Words(int first, int count)
        {
            uint[] words = new uint[count];
            for (int index = 0; index < count; index++)
            {
                words[index] = BoardBuses.Word(BoardBuses.Gpio, 0, first + index);
            }
            return words;
        }

        /// <summary>Why bus <paramref name="bus"/> of <paramref name="kind"/> has no driver.</summary>
        internal static string Describe(int kind, int bus)
        {
            if (kind == BoardBuses.Gpio || BoardBuses.Length(kind, bus) < 0) return BoardTableReport.Describe(kind, bus);
            return "the " + Family + " build of System.Device.Gpio carries no " + (kind == BoardBuses.I2c ? "I2C" : "SPI") + " driver";
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
