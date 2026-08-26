// Lamella System.Device.Gpio -- a separate assembly matching Microsoft's dotnet/iot GPIO API.
namespace System.Device.Gpio
{
    /// <summary>The result of a <see cref="GpioController.WaitForEvent(int, PinEventTypes, System.TimeSpan)"/> call.</summary>
    public struct WaitForEventResult
    {
        /// <summary>The event types that were triggered, when the wait did not time out.</summary>
        public PinEventTypes EventTypes;

        /// <summary>Whether the wait ended because its timeout elapsed rather than because an edge arrived.</summary>
        public bool TimedOut;
    }
}
