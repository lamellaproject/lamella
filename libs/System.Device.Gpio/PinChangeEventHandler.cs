// Lamella System.Device.Gpio -- a separate assembly matching Microsoft's dotnet/iot GPIO API.
namespace System.Device.Gpio
{
    /// <summary>Defines the structure for callbacks when a pin value changed event occurs.</summary>
    /// <param name="sender">The sender of the event.</param>
    /// <param name="pinValueChangedEventArgs">The pin value changed arguments from the event.</param>
    public delegate void PinChangeEventHandler(object sender, PinValueChangedEventArgs pinValueChangedEventArgs);
}
