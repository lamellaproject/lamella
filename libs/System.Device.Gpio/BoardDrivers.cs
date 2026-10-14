// Lamella.Hardware -- the drivers a bus table names, for the build that carries no family's, in the System.Device.Gpio assembly.
#if !LAMELLA_FAMILY_DRIVERS
using System.Device.Gpio;

namespace Lamella.Hardware
{
    /// <summary>Constructs the drivers a board's bus table names. This build carries no family's
    /// drivers, so it constructs none.</summary>
    internal sealed class BoardDrivers
    {
        private BoardDrivers() { }

        /// <summary>The family whose drivers this build carries; null for none.</summary>
        internal const string Family = null;

        /// <summary>I2C bus <paramref name="bus"/>'s driver; null, as this build has none.</summary>
        internal static I2cDriver CreateI2c(int bus) { return null; }

        /// <summary>SPI bus <paramref name="bus"/>'s driver; null, as this build has none.</summary>
        internal static SpiDriver CreateSpi(int bus) { return null; }

        /// <summary>The GPIO controller's driver; null, as this build has none.</summary>
        internal static GpioDriver CreateGpio() { return null; }

        /// <summary>Why bus <paramref name="bus"/> of <paramref name="kind"/> has no driver here: this
        /// build carries no board's drivers.</summary>
        internal static string Describe(int kind, int bus)
        {
            return "this System.Device.Gpio carries no board's drivers: build the program for its board,"
                + " with --board or <LamellaTargetBoard>, or construct new Lamella.Hardware.TargetBoard()"
                + " before opening a bus";
        }
    }
}
#endif
