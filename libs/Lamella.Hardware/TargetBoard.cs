// Lamella.Hardware.TargetBoard -- the board a program was built for, one class on every board.
using System.Collections.Generic;
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

        private BoardPins _pins;

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

        /// <summary>The board's id, the name the build selected it by: "raspberrypi-pico-2-w" on a Raspberry Pi
        /// Pico 2 W.</summary>
        public string Name
        {
            get { return TargetBoardFacts.Name; }
        }

        /// <summary>The board's maker, as the board names it: "Raspberry Pi".</summary>
        public string Maker
        {
            get { return TargetBoardFacts.Maker; }
        }

        /// <summary>The board's product name, as its maker names it: "Pico 2 W".</summary>
        public string Product
        {
            get { return TargetBoardFacts.Product; }
        }

        /// <summary>The board's chip: "RP2350A".</summary>
        public string Chip
        {
            get { return TargetBoardFacts.Chip; }
        }

        /// <summary>The frequency in hertz the board's image runs the chip's processor at.</summary>
        /// <exception cref="System.NotSupportedException">The board's facts do not state it. The message
        /// names the board and the fact.</exception>
        public int ClockFrequency
        {
            get
            {
                int clock = TargetBoardFacts.ClockFrequency;
                if (clock == 0) throw new System.NotSupportedException(NotStated("clock frequency"));
                return clock;
            }
        }

        /// <summary>The pins of the board's chip, by the numbers <c>GpioController.OpenPin</c> takes and
        /// by the labels the board prints.</summary>
        /// <exception cref="System.NotSupportedException">The board's facts do not state its pins. The
        /// message names the board and the fact.</exception>
        public BoardPins Pins
        {
            get
            {
                if (TargetBoardFacts.PinCount == 0) throw new System.NotSupportedException(NotStated("pins"));
                BoardPins pins = _pins;
                if ((object)pins == null)
                {
                    pins = new BoardPins();
                    _pins = pins;
                }
                return pins;
            }
        }

        /// <summary>The I2C buses the board serves, with the rates each serves.</summary>
        /// <exception cref="System.NotSupportedException">The board's facts do not state its buses. The
        /// message names the board and the fact.</exception>
        public IReadOnlyList<I2cBusInfo> I2cBuses
        {
            get
            {
                if (TargetBoardFacts.PinCount == 0) throw new System.NotSupportedException(NotStated("I2C buses"));
                return FactsTable.I2cBuses;
            }
        }

        /// <summary>The SPI buses the board serves, with the slowest and fastest rate each
        /// makes.</summary>
        /// <exception cref="System.NotSupportedException">The board's facts do not state its buses. The
        /// message names the board and the fact.</exception>
        public IReadOnlyList<SpiBusInfo> SpiBuses
        {
            get
            {
                if (TargetBoardFacts.PinCount == 0) throw new System.NotSupportedException(NotStated("SPI buses"));
                return FactsTable.SpiBuses;
            }
        }

        /// <summary>Why a member answers nothing: the board, and the fact its facts do not state.</summary>
        private static string NotStated(string fact)
        {
            return Board() + ": this board's facts do not state its " + fact + ".";
        }

        /// <summary>One line naming the board, its chip and clock, how many pins it numbers and the buses
        /// it serves: "Raspberry Pi Pico 2 (raspberrypi-pico-2), RP2350A at 150000000 Hz, 30 pins, I2C 0, SPI
        /// 0". A fact the board's facts do not state is named as such.</summary>
        public override string ToString()
        {
            string line = Board() + ", " + TargetBoardFacts.Chip;
            int clock = TargetBoardFacts.ClockFrequency;
            if (clock == 0) line = line + ", clock not stated";
            else line = line + " at " + FactsTable.Decimal(clock) + " Hz";
            if (TargetBoardFacts.PinCount == 0) return line + ", pins and buses not stated";
            line = line + ", " + FactsTable.Decimal(TargetBoardFacts.PinCount);
            line = line + " pins, I2C " + FactsTable.ServedI2cNumbers();
            return line + ", SPI " + FactsTable.ServedSpiNumbers();
        }

        /// <summary>The board as a refusal names it: its maker, its product and its id.</summary>
        private static string Board()
        {
            string named = TargetBoardFacts.Maker + " " + TargetBoardFacts.Product;
            return named + " (" + TargetBoardFacts.Name + ")";
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
                    string unclocked = Board() + ": this board's I2C driver for bus " + FactsTable.Decimal(busId);
                    unclocked = unclocked + " sets no clock frequency, so the bus runs at its default rate and "
                        + FactsTable.Decimal(clockFrequency);
                    throw new System.NotSupportedException(unclocked + " Hz cannot be set.");
                }
            }
            else if (!driver.Serves(clockFrequency))
            {
                string unserved = Board() + ": an I2C rate of " + FactsTable.Decimal(clockFrequency);
                unserved = unserved + " Hz on bus " + FactsTable.Decimal(busId);
                unserved = unserved + " is not served by this board. The bus serves " + Listed(rates);
                throw new System.NotSupportedException(unserved + " Hz.");
            }
            int running = Buses.RunI2c(busId, clockFrequency);
            if (running != clockFrequency)
            {
                string busy = Board() + ": I2C bus " + FactsTable.Decimal(busId);
                busy = busy + " already runs at " + FactsTable.Decimal(running);
                busy = busy + " Hz for this program's run; a bus has one rate per run, set by the first device created"
                    + " on it, and " + FactsTable.Decimal(clockFrequency);
                throw new System.InvalidOperationException(busy + " Hz was asked.");
            }
            return I2cDevice.Create(connectionSettings);
        }

        /// <summary><paramref name="rates"/>, in order and separated by commas.</summary>
        private static string Listed(int[] rates)
        {
            string listed = rates.Length == 0 ? "no rate" : FactsTable.Decimal(rates[0]);
            for (int index = 1; index < rates.Length; index++)
            {
                listed = listed + ", " + FactsTable.Decimal(rates[index]);
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
                throw new System.NotSupportedException(Board() + " serves no analog input.");
            }
            if (chip != 0)
            {
                throw new System.NotSupportedException(Board() + " has no analog controller "
                    + FactsTable.Decimal(chip) + "; its one controller is chip 0.");
            }
            return AdcControllers.CreateAnalogController();
        }

#if LAMELLA_SURFACE_FLOAT
        /// <summary>A PWM channel the board drives a pin with, not yet started.</summary>
        /// <param name="chip">The PWM chip, the <see cref="BoardPin.PwmChip"/> of the pin to drive.</param>
        /// <param name="channel">The channel on that chip, the <see cref="BoardPin.PwmChannelNumber"/> of
        /// the pin to drive.</param>
        /// <param name="frequency">The frequency in hertz.</param>
        /// <param name="dutyCyclePercentage">The duty cycle, from 0 to 1.</param>
        /// <exception cref="System.NotSupportedException">No pin of the board carries the chip and
        /// channel, or the board keeps that pin for a part of its own. The message names the board and
        /// what was asked.</exception>
        /// <exception cref="System.InvalidOperationException">The chip's output that drives the pin is
        /// already open, or its other output runs at another frequency.</exception>
        /// <exception cref="System.ArgumentOutOfRangeException">The board's facts state no pin table, so
        /// the numbers are the ones <c>PwmChannel.Create</c> takes, and the board binds no such chip or
        /// channel.</exception>
        public System.Device.Pwm.PwmChannel CreatePwmChannel(int chip, int channel, int frequency = 400,
            double dutyCyclePercentage = 0.5)
        {
            if (TargetBoardFacts.PinCount == 0)
            {
                return System.Device.Pwm.PwmChannel.Create(chip, channel, frequency, dutyCyclePercentage);
            }
            return TargetBoardFacts.CreatePwmOn(FindPwm(chip, channel), frequency, dutyCyclePercentage);
        }

        /// <summary>A PWM channel on the pin <paramref name="pin"/> names, not yet started: a channel drives
        /// one pin of the board, so another pin is refused.</summary>
        /// <param name="chip">The PWM chip, the <see cref="BoardPin.PwmChip"/> of the pin to drive.</param>
        /// <param name="channel">The channel on that chip, the <see cref="BoardPin.PwmChannelNumber"/> of
        /// the pin to drive.</param>
        /// <param name="frequency">The frequency in hertz.</param>
        /// <param name="dutyCyclePercentage">The duty cycle, from 0 to 1.</param>
        /// <param name="pin">The pin the channel drives, by the number <c>GpioController.OpenPin</c>
        /// takes.</param>
        /// <exception cref="System.NotSupportedException">No pin of the board carries the chip and
        /// channel, the board keeps that pin for a part of its own, they drive another pin than
        /// <paramref name="pin"/>, or the board's facts do not state its pins. The message names the
        /// board and what was asked.</exception>
        /// <exception cref="System.InvalidOperationException">The chip's output that drives the pin is
        /// already open, or its other output runs at another frequency.</exception>
        public System.Device.Pwm.PwmChannel CreatePwmChannel(int chip, int channel, int frequency,
            double dutyCyclePercentage, int pin)
        {
            if (TargetBoardFacts.PinCount == 0) throw new System.NotSupportedException(NotStated("pins"));
            int carrier = FindPwm(chip, channel);
            if (pin != carrier)
            {
                string elsewhere = Board() + ": PWM chip " + FactsTable.Decimal(chip);
                elsewhere = elsewhere + ", channel " + FactsTable.Decimal(channel);
                elsewhere = elsewhere + " drives " + FactsTable.FirstLabel(carrier);
                elsewhere = elsewhere + " alone on this board, so it cannot be created on pin " + FactsTable.Decimal(pin);
                throw new System.NotSupportedException(elsewhere + ".");
            }
            return TargetBoardFacts.CreatePwmOn(carrier, frequency, dutyCyclePercentage);
        }

        /// <summary>The number of the pin that carries PWM <paramref name="chip"/>,
        /// <paramref name="channel"/>, refused where no pin carries it or the board keeps the pin.</summary>
        private static int FindPwm(int chip, int channel)
        {
            int count = TargetBoardFacts.PinCount;
            for (int pin = 0; pin < count; pin++)
            {
                int storedChip = FactsTable.StoredPwmChip(pin);
                int storedChannel = FactsTable.StoredPwmChannel(pin);
                if (storedChip != 0 && storedChannel != 0 && storedChip - 1 == chip && storedChannel - 1 == channel)
                {
                    if (TargetBoardFacts.IsPinReserved(pin))
                    {
                        string kept = Board() + ": the board keeps " + FactsTable.FirstLabel(pin);
                        throw new System.NotSupportedException(kept + " for a part of its own, so no PWM channel can drive it.");
                    }
                    return pin;
                }
            }
            string uncarried = Board() + ": no pin carries PWM chip " + FactsTable.Decimal(chip);
            uncarried = uncarried + ", channel " + FactsTable.Decimal(channel);
            throw new System.NotSupportedException(uncarried + " on this board, so no PWM channel can be created there.");
        }
#endif

        /// <summary>Why the board has no <paramref name="kind"/> bus <paramref name="busId"/>: the
        /// board, the bus asked for, and the buses of that kind it has.</summary>
        private static string NoSuchBus(string kind, int busId, bool i2c)
        {
            string message = Board() + " has no " + kind;
            message = message + " bus " + FactsTable.Decimal(busId);
            string listed = null;
            for (int candidate = 0; candidate < Buses.BusCount; candidate++)
            {
                if (!(i2c ? Buses.ServesI2c(candidate) : Buses.ServesSpi(candidate))) continue;
                listed = (object)listed == null ? FactsTable.Decimal(candidate) : listed + ", " + FactsTable.Decimal(candidate);
            }
            if ((object)listed == null) return message + "; it has no " + kind + " bus.";
            message = message + "; its " + kind;
            return message + " buses: " + listed + ".";
        }
    }
}
