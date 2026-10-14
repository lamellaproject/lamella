// System.Device.Pwm -- the dotnet/iot PWM surface: abstract PwmChannel : IDisposable (Frequency / DutyCycle / Start / Stop + the static Create factory), built into the System.Device.Gpio assembly, where upstream ships it.
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
        /// <param name="frequency">The frequency in hertz: 400 when it is left out.</param>
        /// <param name="dutyCyclePercentage">The duty cycle percentage represented as a value between
        /// 0.0 and 1.0: 0.5 when it is left out.</param>
        /// <returns>A new channel, which the caller owns and disposes.</returns>
        /// <exception cref="System.ArgumentOutOfRangeException"><paramref name="chip"/> is out of
        /// range, the chip has no such channel, or the frequency or duty cycle is outside what the
        /// channel can produce.</exception>
        /// <exception cref="System.InvalidOperationException">The board bound no factory for the
        /// chip, or the channel is already open.</exception>
        public static PwmChannel Create(int chip, int channel, int frequency = 400, double dutyCyclePercentage = 0.5)
        {
            return Lamella.Hardware.Buses.CreatePwmChannel(chip, channel, frequency, dutyCyclePercentage);
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
