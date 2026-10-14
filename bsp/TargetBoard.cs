// Lamella.Hardware.TargetBoard -- the board a program was built for, one class on every board.
using System.Device.Gpio;
using Lamella.Generated;

namespace Lamella.Hardware
{
    /// <summary>The board this program was built for.</summary>
    /// <remarks>
    /// <para>Every board's assembly carries this class with that board's own facts, so the board a
    /// program constructs is the one whose assembly it was built against.</para>
    /// <para>Constructing it binds the board's buses, so <see cref="GpioController"/>,
    /// <c>I2cDevice.Create</c> and <c>SpiDevice.Create</c> reach the board's drivers. It is cheap and
    /// holds nothing exclusive: every instance answers the same, and the buses are bound once however
    /// many a program constructs.</para>
    /// </remarks>
    public sealed class TargetBoard
    {
        /// <summary>The board the program was built for, with its buses bound.</summary>
        public TargetBoard()
        {
            TargetBoardFacts.Arm();
        }

        /// <summary>The board's id, the name the build selected it by: "rpi-pico2-w" on a Raspberry Pi
        /// Pico 2 W.</summary>
        public string Name
        {
            get { return TargetBoardFacts.Name; }
        }

        /// <summary>The standard controller over the board's pins.</summary>
        public GpioController CreateGpioController()
        {
            return new GpioController();
        }
    }
}
