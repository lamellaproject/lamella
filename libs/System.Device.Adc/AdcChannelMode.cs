// Lamella System.Device.Adc -- a separate assembly matching the Windows.Devices.Adc /
namespace System.Device.Adc
{
    /// <summary>Describes the channel modes that the ADC controller can use for input.</summary>
    public enum AdcChannelMode
    {
        /// <summary>Simple value of a particular pin.</summary>
        SingleEnded = 0,

        /// <summary>Difference between two pins.</summary>
        Differential = 1
    }
}
