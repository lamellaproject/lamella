// Lamella.Hardware -- the board-populated ADC driver binding, in the nanoFramework ADC assembly.
using System.Device.Adc;

namespace Lamella.Hardware
{
    /// <summary>Creates the ADC driver, on first use.</summary>
    public delegate AdcDriver AdcDriverFactory();

    /// <summary>The board's ADC driver binding. A board binds it once at startup;
    /// <see cref="System.Device.Adc.AdcController"/>'s parameterless constructor then resolves
    /// through here.</summary>
    public sealed class AdcControllers
    {
        private AdcControllers() { }

        private static AdcDriverFactory _factory;
        private static AdcDriver _driver;

        /// <summary>Binds the factory that creates the board's ADC driver. Call it once during
        /// board startup; the factory does not run until an <see cref="AdcController"/> is first
        /// constructed.</summary>
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

        /// <summary>Creates a controller over <paramref name="driver"/> directly, for the cases
        /// that do not go through the board's binding.</summary>
        public static AdcController Create(AdcDriver driver)
        {
            if ((object)driver == null) throw new System.ArgumentNullException("driver");
            return new AdcController(driver);
        }

        internal static AdcDriver Resolve()
        {
            if ((object)_driver != null) return _driver;
            if ((object)_factory == null)
            {
                throw new System.InvalidOperationException(
                    "no ADC driver is bound; the board must call Lamella.Hardware.AdcControllers.Bind at startup");
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
