// The descriptor an RP2350 SAR-ADC driver consumes: the converter, the board's reference rail, and the
// channel map of the board's package -- which GPIO each channel reads, which channel is the temperature
// sensor, which channels the board gives to other lines, and which channels' header pins also land on
// a second GPIO.
namespace Lamella.Boards
{
    public sealed class Rp2350AdcBinding
    {
        /// <summary>The ADC block's base address.</summary>
        public readonly uint AdcBase;
        /// <summary>The ADC's reset-release mask (the converter releases alone; its inputs
        /// are analog pads, not IO-bank routes).</summary>
        public readonly uint ResetMask;
        /// <summary>The board's ADC_VREF/ADC_AVDD rail in microvolts -- what a raw count
        /// converts against.</summary>
        public readonly uint ReferenceMicrovolts;
        /// <summary>The converter's channels on the board's package: its pin channels, then the
        /// temperature sensor.</summary>
        public readonly int ChannelCount;
        /// <summary>The temperature sensor's channel, the last one.</summary>
        public readonly int TemperatureChannel;
        /// <summary>The GPIO each pin channel reads, indexed by channel.</summary>
        public readonly int[] ChannelPins;
        /// <summary>One bit per channel whose pin the board gives to another line.</summary>
        public readonly uint ReservedChannels;
        /// <summary>For each reserved channel, the line its pin belongs to; null for a free
        /// channel.</summary>
        public readonly string[] ReservedBy;
        /// <summary>For each channel whose header pin also lands on a second GPIO, that GPIO's
        /// IO_BANK0 CTRL address; 0 for a channel without one.</summary>
        public readonly uint[] TwinIoCtrl;
        /// <summary>For each channel whose header pin also lands on a second GPIO, that GPIO's
        /// PADS_BANK0 register; 0 for a channel without one.</summary>
        public readonly uint[] TwinPads;

        public Rp2350AdcBinding(uint adcBase, uint resetMask, uint referenceMicrovolts, int channelCount,
            int temperatureChannel, int[] channelPins, uint reservedChannels, string[] reservedBy,
            uint[] twinIoCtrl, uint[] twinPads)
        {
            AdcBase = adcBase;
            ResetMask = resetMask;
            ReferenceMicrovolts = referenceMicrovolts;
            ChannelCount = channelCount;
            TemperatureChannel = temperatureChannel;
            ChannelPins = channelPins;
            ReservedChannels = reservedChannels;
            ReservedBy = reservedBy;
            TwinIoCtrl = twinIoCtrl;
            TwinPads = twinPads;
        }
    }
}
