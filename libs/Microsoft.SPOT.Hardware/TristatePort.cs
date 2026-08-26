// Microsoft.SPOT.Hardware (a .NET Micro Framework compatibility assembly) -- TristatePort.
namespace Microsoft.SPOT.Hardware
{
    /// <summary>A port that can be switched between driving its pin and reading it.</summary>
    public sealed class TristatePort : OutputPort
    {
        /// <summary>Opens <paramref name="portId"/> as a tristate port, not driving.</summary>
        /// <param name="portId">The pin to open.</param>
        /// <param name="initialState">The level to drive once the port is made active.</param>
        /// <param name="glitchFilter">Whether to request the pin's glitch filter.</param>
        /// <param name="resistor">The pull resistor to apply while the port is not driving.</param>
        public TristatePort(Cpu.Pin portId, bool initialState, bool glitchFilter, Port.ResistorMode resistor)
            : base(portId, initialState, glitchFilter, resistor)
        {
        }

        /// <summary>Whether this port is driving its pin (true) or reading it (false).</summary>
        public bool Active
        {
            get { return PortRegistry.Slot(Id).Active; }
            set
            {
                PinState state = PortRegistry.Slot(Id);
                if (state.Active == value)
                {
                    return;
                }
                state.Active = value;
                state.Output = value;
                if (value)
                {
                    PortRegistry.Controller.SetPinMode(state.Pin, System.Device.Gpio.PinMode.Output);
                    PortRegistry.Controller.Write(
                        state.Pin,
                        state.InitialState ? System.Device.Gpio.PinValue.High : System.Device.Gpio.PinValue.Low);
                }
                else
                {
                    PortRegistry.Controller.SetPinMode(state.Pin, PortRegistry.ModeFor(state.Resistor));
                }
            }
        }

        /// <summary>The pull resistor this port applies while it is not driving.</summary>
        public Port.ResistorMode Resistor
        {
            get { return PortRegistry.Slot(Id).Resistor; }
            set
            {
                PinState state = PortRegistry.Slot(Id);
                state.Resistor = value;
                if (!state.Active)
                {
                    PortRegistry.Controller.SetPinMode(state.Pin, PortRegistry.ModeFor(value));
                }
            }
        }

        /// <summary>Whether this port requested its pin's glitch filter.</summary>
        /// <remarks>IMPORTANT: this build records the request and no driver applies it, so a pin is
        /// never actually filtered.</remarks>
        public bool GlitchFilter { get { return PortRegistry.Slot(Id).GlitchFilter; } }
    }
}
