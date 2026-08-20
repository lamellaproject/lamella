// Lamella.Hardware -- the board-populated ADC driver binding.
namespace Lamella.Hardware
{
    /// <summary>Creates the ADC driver, on first use.</summary>
    public delegate AdcDriver AdcDriverFactory();

    /// <summary>The board's ADC driver binding. A board binds it once at startup, and everything
    /// that reads a converter -- <see cref="Adc"/>, and the compatibility surfaces built over it --
    /// resolves through here.</summary>
    public sealed class AdcControllers
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

        /// <summary>The bound ADC driver, creating it on first use.</summary>
        /// <remarks>
        /// <para>THE SAME INSTANCE <see cref="Adc"/> and the compatibility surface use. One
        /// converter has one driver, and this caches it after the first call, so a reading taken
        /// through <see cref="Adc"/> and one taken through a driver from here act on the same
        /// registers.</para>
        /// <para>That guarantee is the reason this is public. Code doing bring-up or a self-test
        /// often needs BOTH the portable entry points and a control that only the concrete driver
        /// exposes -- a reference rail, a raw register, a calibration step. Without this the only
        /// way to reach the driver is to construct a SECOND one over the same converter, which
        /// reads as working and leaves the entry points talking to the first.</para>
        /// <para>Ordinary reading should go through <see cref="Adc"/>. Reach for this when you need
        /// something it cannot express, and cast to the driver type your board bound.</para>
        /// </remarks>
        /// <exception cref="System.InvalidOperationException">No ADC driver is bound.</exception>
        public static AdcDriver Resolve()
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
