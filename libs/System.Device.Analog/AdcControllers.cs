// Lamella.Hardware -- the board-populated ADC driver binding, in the System.Device.Analog assembly.
namespace Lamella.Hardware
{
    /// <summary>Creates the ADC driver, on first use.</summary>
    internal delegate AdcDriver AdcDriverFactory();

    /// <summary>The board's ADC driver binding. A board binds it once at startup, and everything
    /// that reads a converter -- <see cref="Adc"/> and every controller from
    /// <see cref="CreateAnalogController"/> -- resolves through here.</summary>
    internal sealed class AdcControllers
    {
        private AdcControllers() { }

        private static AdcDriverFactory _factory;
        private static AdcDriver _driver;

        /// <summary>Binds the factory that creates the board's ADC driver. Call it once during
        /// board startup; the factory does not run until a converter is first read.</summary>
        public static void Bind(AdcDriverFactory factory)
        {
            if ((object)factory == null) throw new System.ArgumentNullException("factory");
            if ((object)_factory != null)
            {
                throw new System.InvalidOperationException("an ADC driver is already bound");
            }
            _factory = factory;
        }

        /// <summary>Whether an ADC driver is bound.</summary>
        public static bool IsBound()
        {
            return (object)_factory != null;
        }

        /// <summary>A <see cref="System.Device.Analog.AnalogController"/> over the bound ADC driver,
        /// whose pins are the board's pins, numbered as the board numbers them: each is a pin the
        /// driver says one of its channels reads.</summary>
        /// <remarks>Creating one touches no hardware and resolves nothing: the driver is resolved,
        /// and the converter brought up, by the first call that needs it. Every controller created
        /// here, and every other surface over the binding, acts on the same driver instance. With no
        /// driver bound, the controller has no analog pins.</remarks>
        public static System.Device.Analog.AnalogController CreateAnalogController()
        {
            return new AdcAnalogController();
        }

        /// <summary>The bound ADC driver, creating it on first use.</summary>
        /// <remarks>THE SAME INSTANCE <see cref="Adc"/> and the analog controllers use. One converter
        /// has one driver, and this caches it after the first call, so every surface over the binding
        /// acts on the same registers.</remarks>
        /// <exception cref="System.InvalidOperationException">No ADC driver is bound.</exception>
        public static AdcDriver Resolve()
        {
            if ((object)_driver != null) return _driver;
            if ((object)_factory == null)
            {
                throw new System.InvalidOperationException(
                    "no ADC driver is bound: construct the board the program was built for before"
                    + " opening a converter, new Lamella.Hardware.TargetBoard(), which binds its converter"
                    + " where it has one");
            }
            AdcDriver created = _factory();
            if ((object)created == null)
            {
                throw new System.InvalidOperationException("the bound ADC driver factory returned null");
            }
            _driver = created;
            return created;
        }
    }
}
