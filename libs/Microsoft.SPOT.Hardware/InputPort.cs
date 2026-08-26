// Microsoft.SPOT.Hardware (a .NET Micro Framework compatibility assembly) -- InputPort.
namespace Microsoft.SPOT.Hardware
{
    /// <summary>A general-purpose input port.</summary>
    public class InputPort : Port
    {
        /// <summary>Opens <paramref name="portId"/> for reading.</summary>
        /// <param name="portId">The pin to open.</param>
        /// <param name="glitchFilter">Whether to request the pin's glitch filter.</param>
        /// <param name="resistor">The pull resistor to apply.</param>
        public InputPort(Cpu.Pin portId, bool glitchFilter, Port.ResistorMode resistor)
            : base(portId, glitchFilter, resistor, Port.InterruptMode.InterruptNone)
        {
        }

        /// <summary>Opens <paramref name="portId"/> for reading, with an interrupt mode.</summary>
        /// <param name="portId">The pin to open.</param>
        /// <param name="glitchFilter">Whether to request the pin's glitch filter.</param>
        /// <param name="resistor">The pull resistor to apply.</param>
        /// <param name="interruptMode">Which transitions should raise an interrupt.</param>
        protected InputPort(Cpu.Pin portId, bool glitchFilter, Port.ResistorMode resistor, Port.InterruptMode interruptMode)
            : base(portId, glitchFilter, resistor, interruptMode)
        {
        }

        /// <summary>Opens <paramref name="portId"/> as a tristate port's input half.</summary>
        /// <param name="portId">The pin to open.</param>
        /// <param name="initialState">The level to drive while the port is active.</param>
        /// <param name="glitchFilter">Whether to request the pin's glitch filter.</param>
        /// <param name="resistor">The pull resistor to apply.</param>
        protected InputPort(Cpu.Pin portId, bool initialState, bool glitchFilter, Port.ResistorMode resistor)
            : base(portId, initialState, glitchFilter, resistor)
        {
        }

        /// <summary>The pull resistor this port applies.</summary>
        public Port.ResistorMode Resistor
        {
            get { return PortRegistry.Slot(Id).Resistor; }
            set
            {
                PinState state = PortRegistry.Slot(Id);
                state.Resistor = value;
                PortRegistry.Controller.SetPinMode(state.Pin, PortRegistry.ModeFor(value));
            }
        }

        /// <summary>Whether this port requested its pin's glitch filter.</summary>
        /// <remarks>IMPORTANT: this build records the request and no driver applies it, so a pin is
        /// never actually filtered.</remarks>
        public bool GlitchFilter
        {
            get { return PortRegistry.Slot(Id).GlitchFilter; }
            set { PortRegistry.Slot(Id).GlitchFilter = value; }
        }
    }
}
