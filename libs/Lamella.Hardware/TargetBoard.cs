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
    /// <para>The build names the board, and supplies that board's facts with the program, so the
    /// board a program constructs is the one it was built for.</para>
    /// <para>Constructing it binds the board's buses, so <see cref="GpioController"/>,
    /// <c>I2cDevice.Create</c>, <c>SpiDevice.Create</c> and this class's own members reach the board's
    /// drivers. It is cheap and holds nothing exclusive: every instance answers the same, the buses
    /// are bound once however many a program constructs, and a device it creates belongs to the
    /// caller, who disposes it.</para>
    /// </remarks>
    public sealed class TargetBoard
    {
        /// <summary>The standard I2C clock frequency, 100 kHz: the rate a bus runs at unless a
        /// program asks for another.</summary>
        public const int StandardI2cFrequency = 100000;

        /// <summary>The board the program was built for, with its buses bound.</summary>
        /// <exception cref="System.InvalidOperationException">The program was built for no
        /// board.</exception>
        public TargetBoard()
        {
            if ((object)TargetBoardFacts.Name == null)
            {
                throw new System.InvalidOperationException(
                    "This program was built for no board, so no TargetBoard is known. Name the board"
                    + " with the LamellaTargetBoard property.");
            }
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

        /// <summary>A device on one of the board's I2C buses at the standard rate,
        /// <see cref="StandardI2cFrequency"/>: the one at
        /// <see cref="I2cConnectionSettings.DeviceAddress"/> on the bus
        /// <see cref="I2cConnectionSettings.BusId"/> names, as <c>I2cDevice.Create</c> makes it.</summary>
        /// <remarks>The same as <see cref="CreateI2cDevice(I2cConnectionSettings, int)"/> at
        /// <see cref="StandardI2cFrequency"/>, so a bus that already runs at another rate refuses
        /// it.</remarks>
        /// <param name="connectionSettings">The bus and the device's address.</param>
        /// <exception cref="System.ArgumentNullException"><paramref name="connectionSettings"/> is
        /// null.</exception>
        /// <exception cref="System.NotSupportedException">The board has no such bus. The message names
        /// the board, the bus asked for and the buses the board has.</exception>
        /// <exception cref="System.InvalidOperationException">The bus already runs at another rate
        /// for this program's run.</exception>
        public I2cDevice CreateI2cDevice(I2cConnectionSettings connectionSettings)
        {
            return CreateI2cDevice(connectionSettings, StandardI2cFrequency);
        }

        /// <summary>A device on one of the board's I2C buses at <paramref name="clockFrequency"/>:
        /// the one at <see cref="I2cConnectionSettings.DeviceAddress"/> on the bus
        /// <see cref="I2cConnectionSettings.BusId"/> names, as <c>I2cDevice.Create</c> makes it, on a
        /// bus running at that rate.</summary>
        /// <remarks>A bus runs at one rate for the program's run, set by the first device created on
        /// it: at the rate this method is asked for, or at the standard rate by
        /// <c>I2cDevice.Create</c> and <c>I2cBus.Create</c>. A device at another rate on a bus that
        /// already runs is refused, and a device at the bus's own rate is served.</remarks>
        /// <param name="connectionSettings">The bus and the device's address.</param>
        /// <param name="clockFrequency">The bus's clock frequency in hertz: one of the rates the bus
        /// serves, such as <see cref="StandardI2cFrequency"/>.</param>
        /// <exception cref="System.ArgumentNullException"><paramref name="connectionSettings"/> is
        /// null.</exception>
        /// <exception cref="System.NotSupportedException">The board has no such bus, or the bus does
        /// not serve <paramref name="clockFrequency"/>. The message names the board, the bus and what
        /// was asked, and the buses or the rates the board has.</exception>
        /// <exception cref="System.InvalidOperationException">The bus already runs at another rate
        /// for this program's run. The message names the rate it runs at.</exception>
        public I2cDevice CreateI2cDevice(I2cConnectionSettings connectionSettings, int clockFrequency)
        {
            if ((object)connectionSettings == null)
            {
                throw new System.ArgumentNullException("connectionSettings");
            }
            int busId = connectionSettings.BusId;
            I2cDriver driver = Buses.ServesI2c(busId) ? Buses.CreatedI2c(busId) : null;
            if ((object)driver == null)
            {
                throw new System.NotSupportedException(NoSuchBus("I2C", busId, true));
            }
            int[] rates = driver.Rates;
            if ((object)rates == null)
            {
                if (clockFrequency != StandardI2cFrequency)
                {
                    throw new System.NotSupportedException(TargetBoardFacts.Name + ": this board's I2C driver for bus "
                        + busId.ToString() + " sets no clock frequency, so the bus runs at its default rate and "
                        + clockFrequency.ToString() + " Hz cannot be set.");
                }
            }
            else if (!driver.Serves(clockFrequency))
            {
                throw new System.NotSupportedException(TargetBoardFacts.Name + ": an I2C rate of "
                    + clockFrequency.ToString() + " Hz on bus " + busId.ToString()
                    + " is not served by this board. The bus serves " + Listed(rates) + " Hz.");
            }
            int running = Buses.RunI2c(busId, clockFrequency);
            if (running != clockFrequency)
            {
                throw new System.InvalidOperationException(TargetBoardFacts.Name + ": I2C bus " + busId.ToString()
                    + " already runs at " + running.ToString() + " Hz for this program's run; a bus has one rate"
                    + " per run, set by the first device created on it, and " + clockFrequency.ToString()
                    + " Hz was asked.");
            }
            return I2cDevice.Create(connectionSettings);
        }

        /// <summary><paramref name="rates"/>, in order and separated by commas.</summary>
        private static string Listed(int[] rates)
        {
            string listed = rates.Length == 0 ? "no rate" : rates[0].ToString();
            for (int index = 1; index < rates.Length; index++)
            {
                listed = listed + ", " + rates[index].ToString();
            }
            return listed;
        }

        /// <summary>A device on one of the board's SPI buses: the bus
        /// <see cref="SpiConnectionSettings.BusId"/> names, with the device's own chip select, mode and
        /// clock, as <c>SpiDevice.Create</c> makes it.</summary>
        /// <remarks>A chip select that is not one of the bus's is refused at the device's first
        /// transfer, as <c>SpiDevice.Create</c> refuses it.</remarks>
        /// <param name="connectionSettings">The bus, the chip select and the transfer's
        /// settings.</param>
        /// <exception cref="System.ArgumentNullException"><paramref name="connectionSettings"/> is
        /// null.</exception>
        /// <exception cref="System.NotSupportedException">The board has no such bus. The message names
        /// the board, the bus asked for and the buses the board has.</exception>
        public SpiDevice CreateSpiDevice(SpiConnectionSettings connectionSettings)
        {
            if ((object)connectionSettings == null)
            {
                throw new System.ArgumentNullException("connectionSettings");
            }
            if (!Buses.ServesSpi(connectionSettings.BusId))
            {
                throw new System.NotSupportedException(NoSuchBus("SPI", connectionSettings.BusId, false));
            }
            return SpiDevice.Create(connectionSettings);
        }

        /// <summary>The board's analog-to-digital converter, as the standard analog controller. A pin is
        /// opened by the number the board gives it -- <c>OpenPin(26)</c> reads GP26 on a Raspberry Pi
        /// Pico 2.</summary>
        /// <remarks>Creating it touches no hardware; the first pin opened brings the converter
        /// up.</remarks>
        /// <param name="chip">Must be 0, the board's one converter.</param>
        /// <exception cref="System.NotSupportedException">The board has no converter, or
        /// <paramref name="chip"/> is not 0.</exception>
        public AnalogController CreateAnalogController(int chip)
        {
            if (!AdcControllers.IsBound())
            {
                throw new System.NotSupportedException(TargetBoardFacts.Name + " serves no analog input.");
            }
            if (chip != 0)
            {
                throw new System.NotSupportedException(TargetBoardFacts.Name + " has no analog controller "
                    + chip.ToString() + "; its one controller is chip 0.");
            }
            return AdcControllers.CreateAnalogController();
        }

        /// <summary>Why the board has no <paramref name="kind"/> bus <paramref name="busId"/>: the
        /// board, the bus asked for, and the buses of that kind it has.</summary>
        private static string NoSuchBus(string kind, int busId, bool i2c)
        {
            string message = TargetBoardFacts.Name + " has no " + kind + " bus " + busId.ToString();
            string listed = null;
            for (int candidate = 0; candidate < Buses.BusCount; candidate++)
            {
                if (!(i2c ? Buses.ServesI2c(candidate) : Buses.ServesSpi(candidate))) continue;
                listed = (object)listed == null ? candidate.ToString() : listed + ", " + candidate.ToString();
            }
            if ((object)listed == null) return message + "; it has no " + kind + " bus.";
            return message + "; its " + kind + " buses: " + listed + ".";
        }
    }
}
