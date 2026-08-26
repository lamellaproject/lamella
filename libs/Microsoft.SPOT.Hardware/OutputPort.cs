// Microsoft.SPOT.Hardware (a .NET Micro Framework compatibility assembly) -- OutputPort.
namespace Microsoft.SPOT.Hardware
{
    /// <summary>A general-purpose output port.</summary>
    public class OutputPort : Port
    {
        /// <summary>Opens <paramref name="portId"/> for writing and drives <paramref name="initialState"/>.</summary>
        /// <param name="portId">The pin to open.</param>
        /// <param name="initialState">The level to drive on open.</param>
        public OutputPort(Cpu.Pin portId, bool initialState)
            : base(portId, initialState)
        {
        }

        /// <summary>Opens <paramref name="portId"/> as a tristate port's output half.</summary>
        /// <param name="portId">The pin to open.</param>
        /// <param name="initialState">The level to drive while the port is active.</param>
        /// <param name="glitchFilter">Whether to request the pin's glitch filter.</param>
        /// <param name="resistor">The pull resistor to apply while the port is not driving.</param>
        protected OutputPort(Cpu.Pin portId, bool initialState, bool glitchFilter, Port.ResistorMode resistor)
            : base(portId, initialState, glitchFilter, resistor)
        {
        }

        /// <summary>Drives this port's pin to <paramref name="state"/>.</summary>
        /// <param name="state">True to drive high, false to drive low.</param>
        public void Write(bool state)
        {
            PinState slot = PortRegistry.Slot(Id);
            if (!slot.Output)
            {
                throw new System.InvalidOperationException("the port is not driving");
            }
            PortRegistry.Controller.Write(
                slot.Pin,
                state ? System.Device.Gpio.PinValue.High : System.Device.Gpio.PinValue.Low);
        }

        /// <summary>The level this port drove when it was opened.</summary>
        public bool InitialState { get { return PortRegistry.Slot(Id).InitialState; } }
    }
}
