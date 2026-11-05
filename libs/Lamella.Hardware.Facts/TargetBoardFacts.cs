// Lamella.Hardware.Facts -- what Lamella.Hardware.TargetBoard asks of the board a program was built for, in a build that names no board.
namespace Lamella.Generated
{
    /// <summary>The facts of a build for no board: no name, nothing to arm, and empty tables.</summary>
    internal sealed class TargetBoardFacts
    {
        private TargetBoardFacts() { }

        /// <summary>The board's id; null, as the program was built for no board.</summary>
        internal static string Name
        {
            get { return null; }
        }

        /// <summary>Arms nothing, as there is no board, and answers 0.</summary>
        internal static int Arm()
        {
            return 0;
        }

        /// <summary>The board's maker; null, as there is no board.</summary>
        internal static string Maker
        {
            get { return null; }
        }

        /// <summary>The board's product name; null, as there is no board.</summary>
        internal static string Product
        {
            get { return null; }
        }

        /// <summary>The board's chip; null, as there is no board.</summary>
        internal static string Chip
        {
            get { return null; }
        }

        /// <summary>Whether the board keeps the GPIO line for a part of its own; no, as there is no
        /// board.</summary>
        internal static bool IsPinReserved(int pin)
        {
            return false;
        }

#if LAMELLA_SURFACE_FLOAT
        /// <summary>A PWM channel on a GPIO line, which a build for no board has none of.</summary>
        internal static System.Device.Pwm.PwmChannel CreatePwmOn(int pin, int frequency, double dutyCyclePercentage)
        {
            throw new System.NotSupportedException("this board's facts name no PWM line");
        }
#endif

        /// <summary>The processor's clock frequency in hertz; 0, as there is no board.</summary>
        internal static int ClockFrequency
        {
            get { return 0; }
        }

        /// <summary>How many pins the pin table numbers; 0, as there is no board.</summary>
        internal static int PinCount
        {
            get { return 0; }
        }

        /// <summary>The pin table; empty, as there is no board.</summary>
        internal static string Pins
        {
            get { return ""; }
        }

        /// <summary>The label table; empty, as there is no board.</summary>
        internal static string Labels
        {
            get { return ""; }
        }

        /// <summary>The I2C bus table; empty, as there is no board.</summary>
        internal static string I2cBuses
        {
            get { return ""; }
        }

        /// <summary>The SPI bus table; empty, as there is no board.</summary>
        internal static string SpiBuses
        {
            get { return ""; }
        }
    }
}
