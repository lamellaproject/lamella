// Lamella.Hardware -- what an image's bus table says a board lacks, in every family's build of System.Device.Gpio.
#if LAMELLA_FAMILY_DRIVERS
namespace Lamella.Hardware
{
    /// <summary>Says, from the image's bus table, why a bus has no driver: the image was built for
    /// no board, or the board it was built for has no such bus.</summary>
    internal sealed class BoardTableReport
    {
        private BoardTableReport() { }

        /// <summary>Why bus <paramref name="bus"/> of <paramref name="kind"/> has no driver.</summary>
        internal static string Describe(int kind, int bus)
        {
            if (BoardBuses.Length(kind, bus) < 0)
            {
                return "the program was built for no board, so its image carries no bus table: name its"
                    + " board with --board or <LamellaTargetBoard>";
            }
            if (kind == BoardBuses.Gpio) return "the board it was built for has no GPIO controller";
            string name = kind == BoardBuses.I2c ? "I2C" : "SPI";
            string listed = null;
            int count = 0;
            for (int candidate = 0; candidate < Buses.BusCount; candidate++)
            {
                if (BoardBuses.Length(kind, candidate) <= 0) continue;
                listed = count == 0 ? candidate.ToString() : listed + ", " + candidate.ToString();
                count++;
            }
            if (count == 0) return "the board it was built for has no " + name + " bus";
            string text = "the board it was built for has " + name + (count == 1 ? " bus " : " buses ") + listed;
            if (kind == BoardBuses.I2c && bus == 1 && count == 1)
            {
                text += ", and a Raspberry Pi's header bus 1 is bus " + listed + " here";
            }
            return text;
        }
    }
}
#endif
