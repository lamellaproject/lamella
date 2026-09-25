// Lamella.Hardware -- the board-populated driver table, in the System.Device.Gpio assembly.
using System.Device.Gpio;

namespace Lamella.Hardware
{
    /// <summary>Creates the SPI driver for a bus, on first use.</summary>
    public delegate SpiDriver SpiDriverFactory();

    /// <summary>Creates the I2C driver for a bus, on first use.</summary>
    public delegate I2cDriver I2cDriverFactory();

    /// <summary>Creates the GPIO driver, on first use.</summary>
    public delegate GpioDriver GpioDriverFactory();

#if LAMELLA_SURFACE_FLOAT
    /// <summary>Creates output <paramref name="channel"/> of one PWM chip at
    /// <paramref name="frequency"/> and <paramref name="dutyCyclePercentage"/>, not yet started,
    /// for <see cref="System.Device.Pwm.PwmChannel.Create(int, int, int, double)"/>.</summary>
    /// <remarks>A factory refuses a channel its chip does not have with
    /// <see cref="System.ArgumentOutOfRangeException"/> for <c>channel</c>, a frequency or duty cycle
    /// outside what the channel can produce with <see cref="System.ArgumentOutOfRangeException"/> for
    /// <c>frequency</c> or <c>dutyCyclePercentage</c>, and a channel that is already open with
    /// <see cref="System.InvalidOperationException"/>. Disposing the channel it returned releases it,
    /// so the same channel can then be created again.</remarks>
    public delegate System.Device.Pwm.PwmChannel PwmChannelFactory(int channel, int frequency, double dutyCyclePercentage);
#endif

    /// <summary>The board's map from bus id to the driver that serves it. A board binds its buses
    /// once at startup; <see cref="System.Device.Spi.SpiDevice.Create(System.Device.Spi.SpiConnectionSettings)"/>
    /// and its siblings then resolve through here, so application code uses the standard factories
    /// and never names a driver.</summary>
    public sealed class Buses
    {
        /// <summary>The number of logical buses of each kind, and of PWM chips, so valid ids are 0
        /// to <c>BusCount - 1</c>. A bus id or a chip is a LOGICAL index the board maps onto a
        /// peripheral instance, not a peripheral instance number, which is what keeps this a
        /// framework constant rather than a per-chip fact.</summary>
        public const int BusCount = 8;

        private Buses() { }

        private static readonly SpiDriverFactory[] _spiFactories = new SpiDriverFactory[BusCount];
        private static readonly SpiDriver[] _spiDrivers = new SpiDriver[BusCount];
        private static readonly I2cDriverFactory[] _i2cFactories = new I2cDriverFactory[BusCount];
        private static readonly I2cDriver[] _i2cDrivers = new I2cDriver[BusCount];
        private static GpioDriverFactory _gpioFactory;
        private static GpioDriver _gpioDriver;
#if LAMELLA_SURFACE_FLOAT
        private static readonly PwmChannelFactory[] _pwmFactories = new PwmChannelFactory[BusCount];
#endif

        /// <summary>Binds the factory that creates SPI bus <paramref name="busId"/>'s driver.
        /// Call it once per bus during board startup; the factory does not run until something
        /// first opens that bus.</summary>
        public static void BindSpi(int busId, SpiDriverFactory factory)
        {
            CheckBusId(busId);
            if ((object)factory == null) throw new System.ArgumentNullException("factory");
            if ((object)_spiFactories[busId] != null) throw AlreadyBound("SPI", "bus", busId);
            _spiFactories[busId] = factory;
        }

        /// <summary>Binds the factory that creates I2C bus <paramref name="busId"/>'s driver.</summary>
        public static void BindI2c(int busId, I2cDriverFactory factory)
        {
            CheckBusId(busId);
            if ((object)factory == null) throw new System.ArgumentNullException("factory");
            if ((object)_i2cFactories[busId] != null) throw AlreadyBound("I2C", "bus", busId);
            _i2cFactories[busId] = factory;
        }

        /// <summary>Binds the factory that creates the GPIO driver. There is one GPIO controller
        /// per board, so this takes no id.</summary>
        public static void BindGpio(GpioDriverFactory factory)
        {
            if ((object)factory == null) throw new System.ArgumentNullException("factory");
            if ((object)_gpioFactory != null)
            {
                throw new System.InvalidOperationException("a GPIO driver is already bound");
            }
            _gpioFactory = factory;
        }

#if LAMELLA_SURFACE_FLOAT
        /// <summary>Binds the factory that creates PWM chip <paramref name="chip"/>'s channels.
        /// Call it once per chip during board startup; the factory runs each time
        /// <see cref="System.Device.Pwm.PwmChannel.Create(int, int, int, double)"/> names the
        /// chip.</summary>
        public static void BindPwm(int chip, PwmChannelFactory factory)
        {
            CheckChip(chip);
            if ((object)factory == null) throw new System.ArgumentNullException("factory");
            if ((object)_pwmFactories[chip] != null) throw AlreadyBound("PWM", "chip", chip);
            _pwmFactories[chip] = factory;
        }
#endif

        /// <summary>Whether SPI bus <paramref name="busId"/> has a driver bound.</summary>
        public static bool IsSpiBound(int busId)
        {
            CheckBusId(busId);
            return (object)_spiFactories[busId] != null;
        }

        /// <summary>Whether I2C bus <paramref name="busId"/> has a driver bound.</summary>
        public static bool IsI2cBound(int busId)
        {
            CheckBusId(busId);
            return (object)_i2cFactories[busId] != null;
        }

        /// <summary>Whether a GPIO driver is bound.</summary>
        public static bool IsGpioBound()
        {
            return (object)_gpioFactory != null;
        }

#if LAMELLA_SURFACE_FLOAT
        /// <summary>Whether PWM chip <paramref name="chip"/> has a factory bound.</summary>
        public static bool IsPwmBound(int chip)
        {
            CheckChip(chip);
            return (object)_pwmFactories[chip] != null;
        }
#endif

        /// <summary>The SPI driver bound to bus <paramref name="busId"/>, creating it on first
        /// use.</summary>
        /// <remarks>
        /// <para>THE SAME INSTANCE the standard factories use. One physical bus has one driver, and
        /// this caches it after the first call, so a device from
        /// <see cref="System.Device.Spi.SpiDevice.Create(System.Device.Spi.SpiConnectionSettings)"/>
        /// and a driver from here are the same object acting on the same registers.</para>
        /// <para>That guarantee is the reason this is public. Code doing bring-up or a self-test
        /// often needs BOTH the portable facade and a control that only the concrete driver exposes
        /// -- a loopback bit, a receive drain, the realized clock. Without this the only way to
        /// reach the driver is to construct a SECOND one over the same registers, which reads as
        /// working, leaves the facade talking to the first, and turns a self-test green while
        /// exercising nothing.</para>
        /// <para>Ordinary I/O should go through the facade. Reach for this when you need something
        /// the facade cannot express, and cast to the driver type your board bound.</para>
        /// </remarks>
        /// <exception cref="System.ArgumentOutOfRangeException">The bus id is out of range.</exception>
        /// <exception cref="System.InvalidOperationException">No driver is bound for that bus.</exception>
        public static SpiDriver ResolveSpi(int busId)
        {
            CheckBusId(busId);
            if ((object)_spiDrivers[busId] != null) return _spiDrivers[busId];
            SpiDriverFactory factory = _spiFactories[busId];
            if ((object)factory == null) throw NotBound("SPI", "bus", busId);
            SpiDriver created = factory();
            if ((object)created == null) throw FactoryReturnedNull("SPI", "bus", busId);
            _spiDrivers[busId] = created;
            return created;
        }

        /// <summary>The I2C driver bound to bus <paramref name="busId"/>, creating it on first
        /// use.</summary>
        /// <remarks>The same instance the standard factories use; see
        /// <see cref="ResolveSpi"/> for why that guarantee is what makes this worth exposing.</remarks>
        /// <exception cref="System.ArgumentOutOfRangeException">The bus id is out of range.</exception>
        /// <exception cref="System.InvalidOperationException">No driver is bound for that bus.</exception>
        public static I2cDriver ResolveI2c(int busId)
        {
            CheckBusId(busId);
            if ((object)_i2cDrivers[busId] != null) return _i2cDrivers[busId];
            I2cDriverFactory factory = _i2cFactories[busId];
            if ((object)factory == null) throw NotBound("I2C", "bus", busId);
            I2cDriver created = factory();
            if ((object)created == null) throw FactoryReturnedNull("I2C", "bus", busId);
            _i2cDrivers[busId] = created;
            return created;
        }

        /// <summary>The bound GPIO driver, creating it on first use.</summary>
        /// <remarks>The same instance <see cref="System.Device.Gpio.GpioController"/> uses; see
        /// <see cref="ResolveSpi"/> for why that guarantee is what makes this worth exposing.</remarks>
        /// <exception cref="System.InvalidOperationException">No GPIO driver is bound.</exception>
        public static GpioDriver ResolveGpio()
        {
            if ((object)_gpioDriver != null) return _gpioDriver;
            if ((object)_gpioFactory == null)
            {
                throw new System.InvalidOperationException(
                    "no GPIO driver is bound; the board must call Lamella.Hardware.Buses.BindGpio at startup");
            }
            GpioDriver created = _gpioFactory();
            if ((object)created == null)
            {
                throw new System.InvalidOperationException("the bound GPIO driver factory returned null");
            }
            _gpioDriver = created;
            return created;
        }

#if LAMELLA_SURFACE_FLOAT
        /// <summary>A new channel from the factory bound for <paramref name="chip"/>, set to
        /// <paramref name="frequency"/> and <paramref name="dutyCyclePercentage"/> and not yet
        /// started. Nothing is cached: every call runs the factory.</summary>
        internal static System.Device.Pwm.PwmChannel CreatePwmChannel(int chip, int channel, int frequency, double dutyCyclePercentage)
        {
            CheckChip(chip);
            PwmChannelFactory factory = _pwmFactories[chip];
            if ((object)factory == null) throw NotBound("PWM", "chip", chip);
            System.Device.Pwm.PwmChannel created = factory(channel, frequency, dutyCyclePercentage);
            if ((object)created == null) throw FactoryReturnedNull("PWM", "chip", chip);
            return created;
        }

        private static void CheckChip(int chip)
        {
            if (chip < 0 || chip >= BusCount)
            {
                throw new System.ArgumentOutOfRangeException("chip");
            }
        }
#endif

        private static void CheckBusId(int busId)
        {
            if (busId < 0 || busId >= BusCount)
            {
                throw new System.ArgumentOutOfRangeException("busId");
            }
        }

        private static System.Exception NotBound(string kind, string unit, int id)
        {
            return new System.InvalidOperationException(
                "no " + kind + " driver is bound for " + unit + " " + id.ToString()
                + "; the board must bind it at startup");
        }

        private static System.Exception FactoryReturnedNull(string kind, string unit, int id)
        {
            return new System.InvalidOperationException(
                "the bound " + kind + " driver factory for " + unit + " " + id.ToString() + " returned null");
        }

        private static System.Exception AlreadyBound(string kind, string unit, int id)
        {
            return new System.InvalidOperationException(
                "a " + kind + " driver is already bound for " + unit + " " + id.ToString());
        }
    }
}
