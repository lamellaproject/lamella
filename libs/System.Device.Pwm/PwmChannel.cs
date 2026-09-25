// System.Device.Pwm -- the dotnet/iot PWM surface: abstract PwmChannel : IDisposable (Frequency / DutyCycle / Start / Stop + the static Create factory), shipped as its own compatibility assembly.
namespace System.Device.Pwm
{
    /// <summary>Represents a single PWM channel.</summary>
    public abstract class PwmChannel : System.IDisposable
    {
        /// <summary>The frequency in hertz.</summary>
        public abstract int Frequency { get; set; }

#if LAMELLA_SURFACE_FLOAT
        /// <summary>The duty cycle, from 0.0 to 1.0.</summary>
        public abstract double DutyCycle { get; set; }
#endif

        public abstract void Start();

        public abstract void Stop();

#if LAMELLA_SURFACE_FLOAT
        /// <summary>Creates a PWM channel from the factory the board bound for
        /// <paramref name="chip"/>, set to <paramref name="frequency"/> and
        /// <paramref name="dutyCyclePercentage"/> and not yet started.</summary>
        /// <param name="chip">The PWM chip number: a controller the board numbers.</param>
        /// <param name="channel">The PWM channel number: an output of that chip.</param>
        /// <param name="frequency">The frequency in hertz.</param>
        /// <param name="dutyCyclePercentage">The duty cycle percentage represented as a value between
        /// 0.0 and 1.0.</param>
        /// <returns>A new channel, which the caller owns and disposes.</returns>
        /// <exception cref="System.ArgumentOutOfRangeException"><paramref name="chip"/> is out of
        /// range, the chip has no such channel, or the frequency or duty cycle is outside what the
        /// channel can produce.</exception>
        /// <exception cref="System.InvalidOperationException">The board bound no factory for the
        /// chip, or the channel is already open.</exception>
        public static PwmChannel Create(int chip, int channel, int frequency, double dutyCyclePercentage)
        {
            return Lamella.Hardware.Buses.CreatePwmChannel(chip, channel, frequency, dutyCyclePercentage);
        }

        /// <summary>Overload defaulting the duty cycle to 0.5.</summary>
        public static PwmChannel Create(int chip, int channel, int frequency)
        {
            return Create(chip, channel, frequency, 0.5);
        }

        /// <summary>Overload defaulting to 400 Hz and 0.5 duty cycle.</summary>
        public static PwmChannel Create(int chip, int channel)
        {
            return Create(chip, channel, 400, 0.5);
        }
#endif

        public void Dispose()
        {
            Dispose(true);
        }

        protected virtual void Dispose(bool disposing)
        {
        }
    }
}
