// Microsoft.SPOT.Hardware (a .NET Micro Framework compatibility assembly) -- Port.
namespace Microsoft.SPOT.Hardware
{
    /// <summary>The base of the general-purpose I/O port classes.</summary>
    public class Port : NativeEventDispatcher
    {
        /// <summary>How a port's pin is pulled when nothing drives it.</summary>
        public enum ResistorMode
        {
            /// <summary>No pull resistor -- a floating input.</summary>
            Disabled = 0,
            /// <summary>Pulled toward ground.</summary>
            PullDown = 1,
            /// <summary>Pulled toward the supply.</summary>
            PullUp = 2,
        }

        /// <summary>Which pin transitions raise an interrupt.</summary>
        public enum InterruptMode
        {
            /// <summary>No interrupt.</summary>
            InterruptNone = 0,
            /// <summary>A falling edge.</summary>
            InterruptEdgeLow = 1,
            /// <summary>A rising edge.</summary>
            InterruptEdgeHigh = 2,
            /// <summary>Either edge.</summary>
            InterruptEdgeBoth = 3,
            /// <summary>A high level.</summary>
            InterruptEdgeLevelHigh = 4,
            /// <summary>A low level.</summary>
            InterruptEdgeLevelLow = 5,
        }

        private Cpu.Pin _portId;

        /// <summary>Opens <paramref name="portId"/> as an input.</summary>
        /// <param name="portId">The pin to open.</param>
        /// <param name="glitchFilter">Whether to request the pin's glitch filter.</param>
        /// <param name="resistor">The pull resistor to apply.</param>
        /// <param name="interruptMode">Which transitions should raise an interrupt.</param>
        protected Port(Cpu.Pin portId, bool glitchFilter, ResistorMode resistor, InterruptMode interruptMode)
        {
            _portId = portId;
            PinState state = PortRegistry.Slot(portId);
            state.GlitchFilter = glitchFilter;
            state.Resistor = resistor;
            state.Interrupt = interruptMode;
            state.Output = false;
            PortRegistry.Controller.OpenPin(state.Pin, PortRegistry.ModeFor(resistor));
        }

        /// <summary>Opens <paramref name="portId"/> as an output driving <paramref name="initialState"/>.</summary>
        /// <param name="portId">The pin to open.</param>
        /// <param name="initialState">The level to drive on open.</param>
        protected Port(Cpu.Pin portId, bool initialState)
        {
            _portId = portId;
            PinState state = PortRegistry.Slot(portId);
            state.InitialState = initialState;
            state.Resistor = ResistorMode.Disabled;
            state.Interrupt = InterruptMode.InterruptNone;
            state.GlitchFilter = false;
            state.Active = true;
            state.Output = true;
            PortRegistry.Controller.OpenPin(state.Pin, System.Device.Gpio.PinMode.Output);
            PortRegistry.Controller.Write(state.Pin, initialState ? System.Device.Gpio.PinValue.High : System.Device.Gpio.PinValue.Low);
        }

        /// <summary>Opens <paramref name="portId"/> as a tristate port.</summary>
        /// <param name="portId">The pin to open.</param>
        /// <param name="initialState">The level to drive while the port is active.</param>
        /// <param name="glitchFilter">Whether to request the pin's glitch filter.</param>
        /// <param name="resistor">The pull resistor to apply while the port is not driving.</param>
        protected Port(Cpu.Pin portId, bool initialState, bool glitchFilter, ResistorMode resistor)
        {
            _portId = portId;
            PinState state = PortRegistry.Slot(portId);
            state.InitialState = initialState;
            state.GlitchFilter = glitchFilter;
            state.Resistor = resistor;
            state.Interrupt = InterruptMode.InterruptNone;
            state.Active = false;
            state.Output = false;
            PortRegistry.Controller.OpenPin(state.Pin, PortRegistry.ModeFor(resistor));
        }

        /// <summary>Reads the current level of this port's pin.</summary>
        /// <returns>True when the pin reads high.</returns>
        public bool Read()
        {
            PinState state = PortRegistry.Slot(_portId);
            return PortRegistry.Controller.Read(state.Pin) == System.Device.Gpio.PinValue.High;
        }

        /// <summary>The pin this port was opened on.</summary>
        public Cpu.Pin Id { get { return _portId; } }

        /// <summary>Reserves or releases a pin so another port cannot claim it.</summary>
        /// <param name="pin">The pin to reserve or release.</param>
        /// <param name="fReserve">True to reserve, false to release.</param>
        /// <returns>True when the request was granted; false when the pin was already reserved.</returns>
        public static bool ReservePin(Cpu.Pin pin, bool fReserve)
        {
            PinState state = PortRegistry.Slot(pin);
            if (fReserve && state.Reserved)
            {
                return false;
            }
            state.Reserved = fReserve;
            return true;
        }

        /// <summary>Closes this port's pin, making it available to another port.</summary>
        /// <param name="disposing">True when called from Dispose().</param>
        protected override void Dispose(bool disposing)
        {
            PinState state = PortRegistry.Slot(_portId);
            if (PortRegistry.Controller.IsPinOpen(state.Pin))
            {
                PortRegistry.Controller.ClosePin(state.Pin);
            }
            state.Reserved = false;
        }
    }
}
