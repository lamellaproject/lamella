// Microsoft.SPOT.Hardware (a .NET Micro Framework compatibility assembly) -- the shared pin registry.
namespace Microsoft.SPOT.Hardware
{
    internal class PinState
    {
        internal int Pin;
        internal Port.ResistorMode Resistor;
        internal Port.InterruptMode Interrupt;
        internal bool GlitchFilter;
        internal bool InitialState;
        internal bool Active;
        internal bool Reserved;
        internal bool Output;
    }

    internal sealed class PortRegistry
    {
        private static System.Device.Gpio.GpioController _controller;
        private static PinState[] _pins;

        internal static System.Device.Gpio.GpioController Controller
        {
            get
            {
                if (_controller == null)
                {
                    _controller = new System.Device.Gpio.GpioController();
                }
                return _controller;
            }
        }

        internal static PinState Slot(Cpu.Pin pin)
        {
            int number = (int)pin;
            if (number < 0)
            {
                throw new System.ArgumentException("Cpu.Pin.GPIO_NONE is not a pin");
            }
            if (_pins == null)
            {
                _pins = new PinState[Controller.PinCount];
            }
            if (number >= _pins.Length)
            {
                throw new System.ArgumentException("no such pin on this board");
            }
            if (_pins[number] == null)
            {
                PinState created = new PinState();
                created.Pin = number;
                _pins[number] = created;
            }
            return _pins[number];
        }

        internal static System.Device.Gpio.PinMode ModeFor(Port.ResistorMode resistor)
        {
            if (resistor == Port.ResistorMode.PullUp)
            {
                return System.Device.Gpio.PinMode.InputPullUp;
            }
            if (resistor == Port.ResistorMode.PullDown)
            {
                return System.Device.Gpio.PinMode.InputPullDown;
            }
            return System.Device.Gpio.PinMode.Input;
        }
    }
}
