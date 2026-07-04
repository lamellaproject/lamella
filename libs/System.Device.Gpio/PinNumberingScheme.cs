// Lamella System.Device.Gpio -- a separate assembly matching Microsoft's dotnet/iot GPIO API.
namespace System.Device.Gpio
{
    /// <summary>Different numbering schemes supported by GPIO controllers and drivers.</summary>
    public enum PinNumberingScheme
    {
        /// <summary>The controller's own logical numbering scheme (the driver's native pin numbers).</summary>
        Logical = 0,
        /// <summary>The numbering scheme of the physical board/header pins.</summary>
        Board = 1,
    }
}
