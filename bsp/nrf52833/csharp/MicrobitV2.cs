// Lamella.Boards.MicrobitV2 -- the BBC micro:bit v2 (nRF52833, Cortex-M4) board-support package.
using System.Device.Gpio;
using System.Device.I2c;
using Lamella.Generated;

namespace Lamella.Boards
{
    public sealed class MicrobitV2
    {
        public const int I2cBusHz = 100000;

        public static readonly uint I2cFreqWord100k = Nrf52833I2cFacts.FREQUENCY_K100;
        public static readonly uint I2cFreqWord250k = Nrf52833I2cFacts.FREQUENCY_K250;
        public static readonly uint I2cFreqWord400k = Nrf52833I2cFacts.FREQUENCY_K400;

        public static readonly int AccelerometerAddress = Lsm303Agr.AccelAddress;
        public static readonly int MagnetometerAddress = Lsm303Agr.MagAddress;

        /// <summary>The internal I2C bus (TWI0) as the layer-1 driver, configured for
        /// <see cref="I2cBusHz"/>. The on-board sensors and the KL27 interface chip share it.</summary>
        public I2cDriver CreateI2cBus()
        {
            Nrf52833I2cDriver bus = new Nrf52833I2cDriver();
            bus.Configure(I2cBusHz);
            return bus;
        }

        /// <summary>The on-board LSM303AGR accelerometer/magnetometer, wired over a freshly
        /// configured internal bus -- the tilt capstone in one call, versus constructing the bus
        /// and sensor by hand.</summary>
        public Lsm303Agr CreateMotionSensor()
        {
            return new Lsm303Agr(CreateI2cBus());
        }

        /// <summary>The GPIO block (the 5x5 LED matrix rows/columns, buttons A/B).</summary>
        public GpioDriver CreateGpioDriver()
        {
            return new Nrf52833GpioDriver();
        }
    }
}
