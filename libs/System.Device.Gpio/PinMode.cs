// Lamella System.Device.Gpio -- a separate assembly matching Microsoft's dotnet/iot GPIO API.
namespace System.Device.Gpio
{
    /// <summary>Pin modes supported by the GPIO controllers and drivers -- how a pin is configured.</summary>
    public enum PinMode
    {
        /// <summary>Input used for reading values from a pin.</summary>
        Input = 0,
        /// <summary>Output used for writing values to a pin.</summary>
        Output = 1,
        /// <summary>Input using a pull-down resistor.</summary>
        InputPullDown = 2,
        /// <summary>Input using a pull-up resistor.</summary>
        InputPullUp = 3,
    }
}
