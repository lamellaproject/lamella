// Lamella.Hardware.TargetBoard -- the board a program was built for, one class on every board.
using System.Device.Analog;
using System.Device.Gpio;
using System.Device.I2c;
using System.Device.Spi;
using Lamella.Generated;

namespace Lamella.Hardware
{
    /// <summary>The board this program was built for.</summary>
    /// <remarks>
    /// <para>Every board's assembly carries this class with that board's own facts, so the board a
    /// program constructs is the one whose assembly it was built against.</para>
    /// <para>Constructing it binds the board's buses, so <see cref="GpioController"/>,
    /// <c>I2cDevice.Create</c>, <c>SpiDevice.Create</c> and this class's own members reach the board's
    /// drivers. It is cheap and holds nothing exclusive: every instance answers the same, the buses
    /// are bound once however many a program constructs, and a device it creates belongs to the
    /// caller, who disposes it.</para>
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

        /// <summary>A device on one of the board's I2C buses: the one at
        /// <see cref="I2cConnectionSettings.DeviceAddress"/> on the bus
        /// <see cref="I2cConnectionSettings.BusId"/> names, as <c>I2cDevice.Create</c> makes it.</summary>
        /// <param name="connectionSettings">The bus and the device's address.</param>
        /// <exception cref="System.ArgumentNullException"><paramref name="connectionSettings"/> is
        /// null.</exception>
        /// <exception cref="System.IO.IOException">The board has no such bus.</exception>
        public I2cDevice CreateI2cDevice(I2cConnectionSettings connectionSettings)
        {
            if ((object)connectionSettings == null)
            {
                throw new System.ArgumentNullException("connectionSettings");
            }
            return I2cDevice.Create(connectionSettings);
        }

        /// <summary>A device on one of the board's SPI buses: the bus
        /// <see cref="SpiConnectionSettings.BusId"/> names, with the device's own chip select, mode and
        /// clock, as <c>SpiDevice.Create</c> makes it.</summary>
        /// <remarks>A bus the board does not have, or a chip select that is not one of the bus's, is
        /// refused at the device's first transfer, as <c>SpiDevice.Create</c> refuses it.</remarks>
        /// <param name="connectionSettings">The bus, the chip select and the transfer's
        /// settings.</param>
        /// <exception cref="System.ArgumentNullException"><paramref name="connectionSettings"/> is
        /// null.</exception>
        public SpiDevice CreateSpiDevice(SpiConnectionSettings connectionSettings)
        {
            if ((object)connectionSettings == null)
            {
                throw new System.ArgumentNullException("connectionSettings");
            }
            return SpiDevice.Create(connectionSettings);
        }

        /// <summary>The board's analog-to-digital converter, as the standard analog controller. A pin is
        /// opened by the number the board gives it -- <c>OpenPin(26)</c> reads GP26 on a Raspberry Pi
        /// Pico 2 -- and a board with no converter answers a controller with no analog pins.</summary>
        /// <remarks>Creating it touches no hardware; the first pin opened brings the converter up.</remarks>
        /// <param name="chip">Must be 0.</param>
        /// <exception cref="System.ArgumentOutOfRangeException"><paramref name="chip"/> is not 0.</exception>
        public AnalogController CreateAnalogController(int chip)
        {
            if (chip != 0)
            {
                throw new System.ArgumentOutOfRangeException("chip");
            }
            return AdcControllers.CreateAnalogController();
        }
    }
}
