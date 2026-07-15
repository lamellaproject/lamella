// System.Device.Adc -- third-party-compatibility ADC surface shaped after nanoFramework's Windows.Devices.Adc (dotnet/iot ships no core ADC class), shipped as its own assembly.
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
