// Lamella.Hardware -- Lamella.Hardware.AdcChannelMode (the driver seam's conversion mode), in the System.Device.Analog assembly.
namespace Lamella.Hardware
{
    /// <summary>How an ADC driver interprets a channel's input.</summary>
    internal enum AdcChannelMode
    {
        /// <summary>One input, measured against the converter's reference.</summary>
        SingleEnded = 0,

        /// <summary>The difference between two inputs.</summary>
        Differential = 1
    }
}
