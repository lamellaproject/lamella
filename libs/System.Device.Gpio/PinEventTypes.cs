// Lamella System.Device.Gpio -- a separate assembly matching Microsoft's dotnet/iot GPIO API.
namespace System.Device.Gpio
{
    /// <summary>
    /// Event types that can be triggered by the GPIO, also used to report received event types back.
    /// A flags enum so a callback can be registered for both edges (<c>Rising | Falling</c>).
    /// </summary>
    [System.Flags]
    public enum PinEventTypes
    {
        /// <summary>None.</summary>
        None = 0,
        /// <summary>Triggered when the value goes from low to high.</summary>
        Rising = 1,
        /// <summary>Triggered when the value goes from high to low.</summary>
        Falling = 2,
    }
}
