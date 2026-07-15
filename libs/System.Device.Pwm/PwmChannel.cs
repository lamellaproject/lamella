// System.Device.Pwm -- the dotnet/iot PWM surface: abstract PwmChannel : IDisposable (Frequency / DutyCycle / Start / Stop + the static Create factory), shipped as its own compatibility assembly.
namespace System.Device.Pwm
{
    /// <summary>Represents a single PWM channel.</summary>
    public abstract class PwmChannel : System.IDisposable
    {
        /// <summary>The frequency in hertz.</summary>
        public abstract int Frequency { get; set; }

        /// <summary>The duty cycle, from 0.0 to 1.0.</summary>
        public abstract double DutyCycle { get; set; }

        public abstract void Start();

        public abstract void Stop();

        /// <summary>Creates a PwmChannel for the current platform -- not supported on a bare-metal
        /// Lamella target; construct the board's PwmChannel backend directly.</summary>
        public static PwmChannel Create(int chip, int channel, int frequency, double dutyCyclePercentage)
        {
            throw new System.NotSupportedException("There is no host PWM provider on a bare-metal target; construct the board's PwmChannel backend directly.");
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

        public void Dispose()
        {
            Dispose(true);
        }

        protected virtual void Dispose(bool disposing)
        {
        }
    }
}
