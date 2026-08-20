// Lamella.Hardware -- Lamella.Hardware.Adc (the native ADC entry points).
namespace Lamella.Hardware
{
    /// <summary>Reads the board's analog-to-digital converter.</summary>
    /// <remarks>
    /// <para>Every member resolves the board's driver, so the FIRST call to any of them runs the
    /// factory the board bound at startup -- which is where a converter's clock, power and bias are
    /// brought up. Constructing nothing and calling nothing touches no hardware.</para>
    /// <para>Counts are raw. Converting one to a voltage needs the board's reference rail, which
    /// this surface deliberately does not carry: a driver produces counts, and a physical quantity
    /// is constructed only where a caller asks for one.</para>
    /// </remarks>
    public sealed class Adc
    {
        private Adc() { }

        private const int PreparedMaskWidth = 32;
        private static uint _prepared;

        /// <summary>The number of channels the board's converter has, so valid channel numbers are
        /// 0 to <c>ChannelCount - 1</c>.</summary>
        /// <exception cref="System.InvalidOperationException">No ADC driver is bound.</exception>
        public static int ChannelCount
        {
            get { return AdcControllers.Resolve().ChannelCount; }
        }

        /// <summary>The width, in bits, of a conversion result.</summary>
        /// <exception cref="System.InvalidOperationException">No ADC driver is bound.</exception>
        public static int ResolutionInBits
        {
            get { return AdcControllers.Resolve().ResolutionInBits; }
        }

        /// <summary>The lowest count a conversion can report.</summary>
        /// <exception cref="System.InvalidOperationException">No ADC driver is bound.</exception>
        public static int MinValue
        {
            get { return AdcControllers.Resolve().MinValue; }
        }

        /// <summary>The highest count a conversion can report.</summary>
        /// <exception cref="System.InvalidOperationException">No ADC driver is bound.</exception>
        public static int MaxValue
        {
            get { return AdcControllers.Resolve().MaxValue; }
        }

        /// <summary>Whether <paramref name="channel"/> is a channel the board's converter has.
        /// Answers false rather than throwing, so it can be used to test a channel number.</summary>
        /// <exception cref="System.InvalidOperationException">No ADC driver is bound.</exception>
        public static bool IsChannelSupported(int channel)
        {
            if (channel < 0) return false;
            return channel < AdcControllers.Resolve().ChannelCount;
        }

        /// <summary>Performs one conversion on <paramref name="channel"/> and returns the hardware
        /// count, between <see cref="MinValue"/> and <see cref="MaxValue"/>. The channel's
        /// per-channel enable steps run on its first read.</summary>
        /// <exception cref="System.ArgumentOutOfRangeException">The board's converter has no such
        /// channel.</exception>
        /// <exception cref="System.InvalidOperationException">No ADC driver is bound.</exception>
        public static int ReadRaw(int channel)
        {
            AdcDriver driver = AdcControllers.Resolve();
            CheckChannel(driver, channel);
            Prepare(driver, channel);
            return driver.ReadValue(channel);
        }

#if LAMELLA_SURFACE_FLOAT
        /// <summary>Performs one conversion on <paramref name="channel"/> and returns the count as a
        /// fraction of <see cref="MaxValue"/>.</summary>
        /// <exception cref="System.ArgumentOutOfRangeException">The board's converter has no such
        /// channel.</exception>
        /// <exception cref="System.InvalidOperationException">No ADC driver is bound.</exception>
        public static double ReadRatio(int channel)
        {
            AdcDriver driver = AdcControllers.Resolve();
            CheckChannel(driver, channel);
            Prepare(driver, channel);
            return driver.ReadValue(channel) / (double)driver.MaxValue;
        }
#endif

        private static void CheckChannel(AdcDriver driver, int channel)
        {
            if (channel < 0 || channel >= driver.ChannelCount)
            {
                throw new System.ArgumentOutOfRangeException("channel");
            }
        }

        private static void Prepare(AdcDriver driver, int channel)
        {
            if (channel >= PreparedMaskWidth)
            {
                driver.OpenChannel(channel);
                return;
            }
            uint bit = 1u << channel;
            if ((_prepared & bit) == 0u)
            {
                driver.OpenChannel(channel);
                _prepared = _prepared | bit;
            }
        }
    }
}
