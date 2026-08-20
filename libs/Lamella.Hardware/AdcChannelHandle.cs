// Lamella.Hardware -- Lamella.Hardware.AdcChannelHandle (one channel, with `new` and no heap).
namespace Lamella.Hardware
{
    /// <summary>One channel of the board's analog-to-digital converter.</summary>
    /// <remarks>Constructing one touches no hardware and checks nothing -- it names a channel. The
    /// first read brings the converter up and refuses a channel the board does not have.</remarks>
    public struct AdcChannelHandle
    {
        private readonly int _channel;

        /// <summary>Names channel <paramref name="channel"/> of the board's converter.</summary>
        public AdcChannelHandle(int channel)
        {
            _channel = channel;
        }

        /// <summary>The channel this handle names.</summary>
        public int Channel
        {
            get { return _channel; }
        }

        /// <summary>Performs one conversion on this channel and returns the hardware count.</summary>
        /// <exception cref="System.ArgumentOutOfRangeException">The board's converter has no such
        /// channel.</exception>
        /// <exception cref="System.InvalidOperationException">No ADC driver is bound.</exception>
        public int ReadRaw()
        {
            return Adc.ReadRaw(_channel);
        }

#if LAMELLA_SURFACE_FLOAT
        /// <summary>Performs one conversion on this channel and returns the count as a fraction of
        /// the converter's highest count.</summary>
        /// <exception cref="System.ArgumentOutOfRangeException">The board's converter has no such
        /// channel.</exception>
        /// <exception cref="System.InvalidOperationException">No ADC driver is bound.</exception>
        public double ReadRatio()
        {
            return Adc.ReadRatio(_channel);
        }
#endif
    }
}
