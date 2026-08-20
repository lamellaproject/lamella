// Lamella.Hardware -- Lamella.Hardware.AdcChannelMode (the driver seam's conversion mode).
namespace Lamella.Hardware
{
    /// <summary>How an ADC driver interprets a channel's input.</summary>
    public enum AdcChannelMode
    {
        /// <summary>One input, measured against the converter's reference.</summary>
        SingleEnded = 0,

        /// <summary>The difference between two inputs.</summary>
        Differential = 1
    }
}
