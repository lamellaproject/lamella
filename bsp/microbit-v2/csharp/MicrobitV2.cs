// Lamella.Boards.MicrobitV2 -- the BBC micro:bit v2 (nRF52833, Cortex-M4) board-support package.
using System.Device.Gpio;
using System.Device.I2c;
using Lamella.Generated;
using Lamella.Hardware;

namespace Lamella.Boards
{
    public sealed class MicrobitV2
    {
        public const int I2cBusHz = 100000;

        public static readonly uint I2cFreqWord100k = Nrf52833TwiLayout.FREQUENCY_K100;
        public static readonly uint I2cFreqWord250k = Nrf52833TwiLayout.FREQUENCY_K250;
        public static readonly uint I2cFreqWord400k = Nrf52833TwiLayout.FREQUENCY_K400;

        /// <summary>The accelerometer's I2C address on the internal bus.</summary>
        /// <remarks>THE SENSOR PART IS NOT GUARANTEED BY THE BOARD REVISION. The vendor's own
        /// hardware documentation, https://tech.microbit.org/hardware/2-0-revision/ , states:
        /// <para>"The micro:bit has a footprint for two different motion sensors: one made by ST
        /// (the LSM303AGR) and one by NXP (FXOS8700CQ). The micro:bit DAL supports both of these
        /// sensors, detecting them at runtime. Only one sensor will ever be placed."</para>
        /// The footprint is shared, so the placed part is a manufacturing fact rather than a
        /// design guarantee -- which is why the reference runtime DETECTS it. The ST part answers
        /// as two bus residents; the NXP part is a single combined device and does not answer at
        /// <see cref="MagnetometerAddress"/> at all. These constants therefore describe the ST
        /// population, which is every unit measured here. Code that must be correct on ANY unit
        /// should probe WHO_AM_I (the LSM303AGR answers 0x33 at 0x0F and 0x40 at 0x4F) instead of
        /// trusting them.</remarks>
        public static readonly int AccelerometerAddress = (int)MicrobitV2Bindings.LSM303AGR_ADDRESS;
        public static readonly int MagnetometerAddress = (int)MicrobitV2Bindings.LSM303AGR_MAG_ADDRESS;

        /// <summary>The `internal-i2c` binding descriptor, lifted from the generated consts
        /// (one naming scheme -- the role's resolved facts in one construction).</summary>
        public Nrf52833TwiBinding CreateInternalI2cBinding()
        {
            return new Nrf52833TwiBinding(
                MicrobitV2Bindings.INTERNAL_I2C_TWI_BASE,
                MicrobitV2Bindings.INTERNAL_I2C_PSEL_SCL,
                MicrobitV2Bindings.INTERNAL_I2C_PSEL_SDA,
                MicrobitV2Bindings.INTERNAL_I2C_PIN_CNF_SCL_REG,
                MicrobitV2Bindings.INTERNAL_I2C_PIN_CNF_SDA_REG);
        }

        /// <summary>The internal I2C bus (TWI0) as the layer-1 driver, configured for
        /// <see cref="I2cBusHz"/>. The on-board sensors and the KL27 interface chip share it.</summary>
        public I2cDriver CreateI2cBus()
        {
            Nrf52833I2cDriver bus = new Nrf52833I2cDriver(CreateInternalI2cBinding());
            bus.Configure(I2cBusHz);
            return bus;
        }

        /// <summary>The GPIO block (the 5x5 LED matrix rows/columns, buttons A/B).</summary>
        public GpioDriver CreateGpioDriver()
        {
            return new Nrf52833GpioDriver(1u << 6, 1u << 8);
        }
    }
}
