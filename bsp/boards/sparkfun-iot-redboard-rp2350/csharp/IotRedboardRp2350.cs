// Lamella.Boards.Sparkfun.IotRedboardRp2350 -- the SparkFun IoT RedBoard - RP2350 (RP2350B + a
// Raspberry Pi RM2 radio module).
using System;
using System.Device.Analog;
using System.Device.Gpio;
using System.Device.I2c;
using System.Device.Pwm;
using System.Device.Spi;
using Lamella.Generated;
using Lamella.Hardware;

namespace Lamella.Boards.Sparkfun
{
    internal sealed class IotRedboardRp2350
    {
        /// <summary>The wire identity this board advertises (lamella_wire::board_model).</summary>
        public static readonly int BoardModel = SparkfunIotRedboardRp2350Bindings.BOARD_MODEL;

        /// <summary>The blue STAT LED on GP25, lit by a high level -- the board's blink target.</summary>
        public static readonly int LedPin = (int)SparkfunIotRedboardRp2350Bindings.LED0_PIN;

        /// <summary>The data line of the board's addressable RGB LED, GP3.</summary>
        public static readonly int NeopixelPin = (int)SparkfunIotRedboardRp2350Bindings.NEOPIXEL_PIN;

        /// <summary>The bank-0 pin the RM2 radio module owns, its power-on line GP24, as a mask.
        /// Composed from the generated mask rather than written out, so a line moved in board.toml
        /// moves here.</summary>
        public static readonly uint RadioPins = SparkfunIotRedboardRp2350Bindings.RM2_WL_ON_MASK;

        /// <summary>The bank-1 pins the board keeps, as a mask over GP32 to GP47: the radio's chip
        /// select, clock and data lines (GP36 to GP38) and the PSRAM's chip select (GP47).</summary>
        public static readonly uint ReservedBank1Pins =
            SparkfunIotRedboardRp2350Bindings.RM2_CS_BANK1_MASK
            | SparkfunIotRedboardRp2350Bindings.RM2_CLK_BANK1_MASK
            | SparkfunIotRedboardRp2350Bindings.RM2_DATA_BANK1_MASK
            | SparkfunIotRedboardRp2350Bindings.PSRAM_CS_BANK1_MASK;

        /// <summary>Binds this board's buses to the driver table, so a program writes plain
        /// dotnet/iot -- <c>SpiDevice.Create(settings)</c>, <c>new GpioController()</c> -- and never
        /// names a Lamella type. Touching <see cref="IotRedboardRp2350"/> at all is what arms it, and
        /// constructing <see cref="TargetBoard"/> touches it.</summary>
        /// <remarks>A type initializer rather than the instance constructor: the table refuses a
        /// second bind of the same id rather than replacing it, and this class is instantiable and
        /// routinely constructed as a temporary. The language runs a type initializer once per
        /// program. The bound values are factories, not drivers, so a program that never touches a
        /// bus never constructs its driver.</remarks>
        static IotRedboardRp2350()
        {
            Buses.BindSpi(0, new SpiDriverFactory(MakeSpi0));
            Buses.BindI2c(0, new I2cDriverFactory(MakeI2c0));
            Buses.BindGpio(new GpioDriverFactory(MakeGpio));
            AdcControllers.Bind(new AdcDriverFactory(MakeAdc));
#if LAMELLA_SURFACE_FLOAT
            Buses.SetPwmChipCount(SparkfunIotRedboardRp2350Bindings.PWM_CHIP_COUNT);
            Buses.BindPwm(0, new PwmChannelFactory(OpenPwm0));
            Buses.BindPwm(1, new PwmChannelFactory(OpenPwm1));
            Buses.BindPwm(6, new PwmChannelFactory(OpenPwm6));
            Buses.BindPwm(7, new PwmChannelFactory(OpenPwm7));
            Buses.BindPwm(8, new PwmChannelFactory(OpenPwm8));
            Buses.BindPwm(9, new PwmChannelFactory(OpenPwm9));
#endif
        }

        private static SpiDriver MakeSpi0() { return new Rp2350SpiDriver(SpiBinding(0)); }
        private static I2cDriver MakeI2c0() { return new Rp2350I2cDriver(I2cBinding(0)); }
        private static GpioDriver MakeGpio()
        {
            return new Rp2350GpioDriver(RadioPins, ReservedBank1Pins, new int[0], new int[0]);
        }
        private static AdcDriver MakeAdc() { return new Rp2350AdcDriver(AdcBinding()); }

        /// <summary>A GPIO controller over the RP2350 SIO and pads, GP0 to GP47. The radio's lines,
        /// GP24 and GP36 to GP38, and GP47, the PSRAM's chip select, refuse <c>SetPinMode</c>.</summary>
        public GpioController CreateGpioController()
        {
            return new GpioController();
        }

        /// <summary>The on-chip temperature sensor's converter channel.</summary>
        public static readonly int TemperatureSensorChannel = (int)SparkfunIotRedboardRp2350Bindings.ADC_TEMPERATURE_CHANNEL;
        /// <summary>The converter channel that reads GP40, the header's A0.</summary>
        public static readonly int AdcChannelGp40 = (int)SparkfunIotRedboardRp2350Bindings.ADC_GPIO40_CHANNEL;
        /// <summary>The converter channel that reads GP41, the header's A1.</summary>
        public static readonly int AdcChannelGp41 = (int)SparkfunIotRedboardRp2350Bindings.ADC_GPIO41_CHANNEL;
        /// <summary>The converter channel that reads GP42, the header's A2.</summary>
        public static readonly int AdcChannelGp42 = (int)SparkfunIotRedboardRp2350Bindings.ADC_GPIO42_CHANNEL;
        /// <summary>The converter channel that reads GP43, the header's A3.</summary>
        public static readonly int AdcChannelGp43 = (int)SparkfunIotRedboardRp2350Bindings.ADC_GPIO43_CHANNEL;
        /// <summary>The converter channel that reads GP44, the header's A4.</summary>
        public static readonly int AdcChannelGp44 = (int)SparkfunIotRedboardRp2350Bindings.ADC_GPIO44_CHANNEL;
        /// <summary>The converter channel that reads GP45, the header's A5.</summary>
        public static readonly int AdcChannelGp45 = (int)SparkfunIotRedboardRp2350Bindings.ADC_GPIO45_CHANNEL;
        /// <summary>The converter channel that reads GP46, the barrel jack's input through the board's
        /// 41.2k over 10k divider.</summary>
        public static readonly int AdcChannelGp46 = (int)SparkfunIotRedboardRp2350Bindings.ADC_GPIO46_CHANNEL;
        /// <summary>The board's converter reference, in microvolts.</summary>
        public static readonly int AdcReferenceMicrovolts = (int)SparkfunIotRedboardRp2350Bindings.ADC_REFERENCE_UV;

        /// <summary>The `adc` binding descriptor, built from the generated constants.</summary>
        public Rp2350AdcBinding CreateAdcBinding() { return AdcBinding(); }

        private static Rp2350AdcBinding AdcBinding()
        {
            return new Rp2350AdcBinding(
                SparkfunIotRedboardRp2350Bindings.ADC_BASE,
                SparkfunIotRedboardRp2350Bindings.ADC_RESET_MASK,
                SparkfunIotRedboardRp2350Bindings.ADC_REFERENCE_UV,
                (int)SparkfunIotRedboardRp2350Bindings.ADC_CHANNEL_COUNT,
                (int)SparkfunIotRedboardRp2350Bindings.ADC_TEMPERATURE_CHANNEL,
                new int[] {
                    (int)SparkfunIotRedboardRp2350Bindings.ADC_CHANNEL0_PIN, (int)SparkfunIotRedboardRp2350Bindings.ADC_CHANNEL1_PIN,
                    (int)SparkfunIotRedboardRp2350Bindings.ADC_CHANNEL2_PIN, (int)SparkfunIotRedboardRp2350Bindings.ADC_CHANNEL3_PIN,
                    (int)SparkfunIotRedboardRp2350Bindings.ADC_CHANNEL4_PIN, (int)SparkfunIotRedboardRp2350Bindings.ADC_CHANNEL5_PIN,
                    (int)SparkfunIotRedboardRp2350Bindings.ADC_CHANNEL6_PIN, (int)SparkfunIotRedboardRp2350Bindings.ADC_CHANNEL7_PIN },
                SparkfunIotRedboardRp2350Bindings.ADC_RESERVED_CHANNELS,
                new string[] {
                    null, null, null, null, null, null, null,
                    SparkfunIotRedboardRp2350Bindings.ADC_CHANNEL7_RESERVED_BY },
                new uint[8],
                new uint[8]);
        }

        /// <summary>The on-chip converter as dotnet/iot's analog controller. A pin is opened by its GPIO
        /// number, GP40 to GP46: <c>OpenPin(40)</c> reads GP40, the header's A0, converter channel
        /// <see cref="AdcChannelGp40"/>. GP47 is the PSRAM's select, and the temperature sensor's
        /// channel reads no pin, so neither opens here.</summary>
        /// <remarks>Creating it touches no hardware; the first pin opened brings the converter up.
        /// It reads through the converter driver the board binds.</remarks>
        /// <param name="chip">Must be 0: the board has one converter.</param>
        /// <exception cref="ArgumentOutOfRangeException"><paramref name="chip"/> is not 0.</exception>
        public AnalogController CreateAnalogController(int chip)
        {
            if (chip != 0)
            {
                throw new ArgumentOutOfRangeException("chip");
            }
            return AdcControllers.CreateAnalogController();
        }

        /// <summary>The `uart0` binding descriptor (GP0 TX / GP1 RX, crystal-exact clk_peri).</summary>
        public Rp2350UartBinding CreateUartBinding()
        {
            return new Rp2350UartBinding(
                SparkfunIotRedboardRp2350Bindings.UART0_BASE,
                SparkfunIotRedboardRp2350Bindings.UART0_RESET_MASK,
                SparkfunIotRedboardRp2350Bindings.UART0_IO_TX_CTRL,
                SparkfunIotRedboardRp2350Bindings.UART0_IO_RX_CTRL,
                SparkfunIotRedboardRp2350Bindings.UART0_PADS_TX,
                SparkfunIotRedboardRp2350Bindings.UART0_PADS_RX,
                SparkfunIotRedboardRp2350Bindings.UART0_FUNCSEL,
                SparkfunIotRedboardRp2350Bindings.UART0_CLK_PERI_HZ);
        }

        /// <summary>UART0 on GP0 (TX) / GP1 (RX), the header's D1/TX and D0/RX, ready for
        /// <c>Init(baud)</c>.</summary>
        public Rp2350Uart CreateUart()
        {
            return new Rp2350Uart(CreateUartBinding());
        }

        /// <summary>The `spi0` binding descriptor for <paramref name="busId"/>
        /// (bus 0 = SPI0 on GP20 MISO / GP22 SCK / GP23 MOSI, whose chip select 0 is GP21, the
        /// header's D10; unknown ids refuse loudly).</summary>
        public Rp2350SpiBinding CreateSpiBinding(int busId) { return SpiBinding(busId); }

        private static Rp2350SpiBinding SpiBinding(int busId)
        {
            if (busId != 0)
            {
                throw new ArgumentException("sparkfun-iot-redboard-rp2350 has no such spi bus: bus 0 = SPI0 on GP20..GP23");
            }
            return new Rp2350SpiBinding(
                SparkfunIotRedboardRp2350Bindings.SPI0_BASE,
                SparkfunIotRedboardRp2350Bindings.SPI0_RESET_MASK,
                SparkfunIotRedboardRp2350Bindings.SPI0_IO_MISO_CTRL,
                SparkfunIotRedboardRp2350Bindings.SPI0_PADS_MISO,
                SparkfunIotRedboardRp2350Bindings.SPI0_IO_SCK_CTRL,
                SparkfunIotRedboardRp2350Bindings.SPI0_PADS_SCK,
                SparkfunIotRedboardRp2350Bindings.SPI0_IO_MOSI_CTRL,
                SparkfunIotRedboardRp2350Bindings.SPI0_PADS_MOSI,
                SparkfunIotRedboardRp2350Bindings.SPI0_FUNCSEL,
                SparkfunIotRedboardRp2350Bindings.SPI0_SSPCLK_HZ,
                new int[] { (int)SparkfunIotRedboardRp2350Bindings.SPI0_CHIP_SELECT0 });
        }

        /// <summary>A SPI device per <paramref name="settings"/>: the settings' BusId picks
        /// the descriptor. ChipSelectLine is an index into the bus's chip selects: 0 is GP21 on
        /// bus 0, driven as the select around each operation, and -1 is no chip select.</summary>
        public SpiDevice CreateSpiDevice(SpiConnectionSettings settings)
        {
            return SpiDevice.Create(settings);
        }

        /// <summary>A SPI device on bus 0 with <paramref name="chipSelectLine"/>: an index into the
        /// bus's chip selects, or -1 for none.</summary>
        public SpiDevice CreateSpiDevice(int chipSelectLine)
        {
            return CreateSpiDevice(new SpiConnectionSettings(0, chipSelectLine));
        }

        /// <summary>The `i2c0` binding descriptor for <paramref name="busId"/>
        /// (bus 0 = I2C0 on GP4 SDA / GP5 SCL, the header's SDA and SCL, the Qwiic connector and the
        /// fuel gauge; unknown ids refuse loudly).</summary>
        public Rp2350I2cBinding CreateI2cBinding(int busId) { return I2cBinding(busId); }

        private static Rp2350I2cBinding I2cBinding(int busId)
        {
            if (busId != 0)
            {
                throw new ArgumentException("sparkfun-iot-redboard-rp2350 has no such i2c bus: bus 0 = I2C0 on GP4/GP5");
            }
            return new Rp2350I2cBinding(
                SparkfunIotRedboardRp2350Bindings.I2C0_BASE,
                SparkfunIotRedboardRp2350Bindings.I2C0_RESET_MASK,
                SparkfunIotRedboardRp2350Bindings.I2C0_IO_SDA_CTRL,
                SparkfunIotRedboardRp2350Bindings.I2C0_PADS_SDA,
                SparkfunIotRedboardRp2350Bindings.I2C0_IO_SCL_CTRL,
                SparkfunIotRedboardRp2350Bindings.I2C0_PADS_SCL,
                SparkfunIotRedboardRp2350Bindings.I2C0_FUNCSEL,
                SparkfunIotRedboardRp2350Bindings.I2C0_IC_CLK_HZ);
        }

        /// <summary>An I2C device per <paramref name="settings"/>: the settings' BusId picks
        /// the descriptor.</summary>
        public I2cDevice CreateI2cDevice(I2cConnectionSettings settings)
        {
            return I2cDevice.Create(settings);
        }

        /// <summary>An I2C device on bus 0 (I2C0, GP4/GP5) at <paramref name="deviceAddress"/>.</summary>
        public I2cDevice CreateI2cDevice(int deviceAddress)
        {
            return CreateI2cDevice(new I2cConnectionSettings(0, deviceAddress));
        }

#if LAMELLA_SURFACE_FLOAT
        private static readonly Rp2350PwmSlice[] _pwmSlices = new Rp2350PwmSlice[(int)Rp2350PwmLayout.SLICE_COUNT];

        private static PwmChannel OpenPwm(int slice, int channel, int frequency, double dutyCyclePercentage)
        {
            if ((object)_pwmSlices[slice] == null)
            {
                _pwmSlices[slice] = Rp2350PwmSlice.Of(PwmBinding(slice));
            }
            return _pwmSlices[slice].Open(channel, frequency, dutyCyclePercentage);
        }

        private static PwmChannel OpenPwm0(int channel, int frequency, double duty) { return OpenPwm(0, channel, frequency, duty); }
        private static PwmChannel OpenPwm1(int channel, int frequency, double duty) { return OpenPwm(1, channel, frequency, duty); }
        private static PwmChannel OpenPwm6(int channel, int frequency, double duty) { return OpenPwm(6, channel, frequency, duty); }
        private static PwmChannel OpenPwm7(int channel, int frequency, double duty) { return OpenPwm(7, channel, frequency, duty); }
        private static PwmChannel OpenPwm8(int channel, int frequency, double duty) { return OpenPwm(8, channel, frequency, duty); }
        private static PwmChannel OpenPwm9(int channel, int frequency, double duty) { return OpenPwm(9, channel, frequency, duty); }

        /// <summary>The binding descriptor of the board's pwm binding for <paramref name="slice"/>: 0, 1,
        /// 6, 7, 8 or 9. Any other slice refuses loudly.</summary>
        public Rp2350PwmBinding CreatePwmBinding(int slice) { return PwmBinding(slice); }

        private static Rp2350PwmBinding PwmBinding(int slice)
        {
            switch (slice)
            {
                case 0:
                    return new Rp2350PwmBinding(SparkfunIotRedboardRp2350Bindings.PWM0_BASE, SparkfunIotRedboardRp2350Bindings.PWM0_RESET_MASK, SparkfunIotRedboardRp2350Bindings.PWM0_SLICE, SparkfunIotRedboardRp2350Bindings.PWM0_FUNCSEL,
                        SparkfunIotRedboardRp2350Bindings.PWM0_CLK_SYS_HZ, new Rp2350PwmOutput[] {
                            new Rp2350PwmOutput(SparkfunIotRedboardRp2350Bindings.PWM0_OUTPUT_A, SparkfunIotRedboardRp2350Bindings.PWM0_IO_A_CTRL, SparkfunIotRedboardRp2350Bindings.PWM0_PADS_A),
                            new Rp2350PwmOutput(SparkfunIotRedboardRp2350Bindings.PWM0_OUTPUT_B, SparkfunIotRedboardRp2350Bindings.PWM0_IO_B_CTRL, SparkfunIotRedboardRp2350Bindings.PWM0_PADS_B) });
                case 1:
                    return new Rp2350PwmBinding(SparkfunIotRedboardRp2350Bindings.PWM1_BASE, SparkfunIotRedboardRp2350Bindings.PWM1_RESET_MASK, SparkfunIotRedboardRp2350Bindings.PWM1_SLICE, SparkfunIotRedboardRp2350Bindings.PWM1_FUNCSEL,
                        SparkfunIotRedboardRp2350Bindings.PWM1_CLK_SYS_HZ, new Rp2350PwmOutput[] {
                            new Rp2350PwmOutput(SparkfunIotRedboardRp2350Bindings.PWM1_OUTPUT_A, SparkfunIotRedboardRp2350Bindings.PWM1_IO_A_CTRL, SparkfunIotRedboardRp2350Bindings.PWM1_PADS_A),
                            new Rp2350PwmOutput(SparkfunIotRedboardRp2350Bindings.PWM1_OUTPUT_B, SparkfunIotRedboardRp2350Bindings.PWM1_IO_B_CTRL, SparkfunIotRedboardRp2350Bindings.PWM1_PADS_B) });
                case 6:
                    return new Rp2350PwmBinding(SparkfunIotRedboardRp2350Bindings.PWM6_BASE, SparkfunIotRedboardRp2350Bindings.PWM6_RESET_MASK, SparkfunIotRedboardRp2350Bindings.PWM6_SLICE, SparkfunIotRedboardRp2350Bindings.PWM6_FUNCSEL,
                        SparkfunIotRedboardRp2350Bindings.PWM6_CLK_SYS_HZ, new Rp2350PwmOutput[] {
                            new Rp2350PwmOutput(SparkfunIotRedboardRp2350Bindings.PWM6_OUTPUT_A, SparkfunIotRedboardRp2350Bindings.PWM6_IO_A_CTRL, SparkfunIotRedboardRp2350Bindings.PWM6_PADS_A),
                            new Rp2350PwmOutput(SparkfunIotRedboardRp2350Bindings.PWM6_OUTPUT_B, SparkfunIotRedboardRp2350Bindings.PWM6_IO_B_CTRL, SparkfunIotRedboardRp2350Bindings.PWM6_PADS_B) });
                case 7:
                    return new Rp2350PwmBinding(SparkfunIotRedboardRp2350Bindings.PWM7_BASE, SparkfunIotRedboardRp2350Bindings.PWM7_RESET_MASK, SparkfunIotRedboardRp2350Bindings.PWM7_SLICE, SparkfunIotRedboardRp2350Bindings.PWM7_FUNCSEL,
                        SparkfunIotRedboardRp2350Bindings.PWM7_CLK_SYS_HZ, new Rp2350PwmOutput[] {
                            new Rp2350PwmOutput(SparkfunIotRedboardRp2350Bindings.PWM7_OUTPUT_A, SparkfunIotRedboardRp2350Bindings.PWM7_IO_A_CTRL, SparkfunIotRedboardRp2350Bindings.PWM7_PADS_A),
                            new Rp2350PwmOutput(SparkfunIotRedboardRp2350Bindings.PWM7_OUTPUT_B, SparkfunIotRedboardRp2350Bindings.PWM7_IO_B_CTRL, SparkfunIotRedboardRp2350Bindings.PWM7_PADS_B) });
                case 8:
                    return new Rp2350PwmBinding(SparkfunIotRedboardRp2350Bindings.PWM8_BASE, SparkfunIotRedboardRp2350Bindings.PWM8_RESET_MASK, SparkfunIotRedboardRp2350Bindings.PWM8_SLICE, SparkfunIotRedboardRp2350Bindings.PWM8_FUNCSEL,
                        SparkfunIotRedboardRp2350Bindings.PWM8_CLK_SYS_HZ, new Rp2350PwmOutput[] {
                            new Rp2350PwmOutput(SparkfunIotRedboardRp2350Bindings.PWM8_OUTPUT_A, SparkfunIotRedboardRp2350Bindings.PWM8_IO_A_CTRL, SparkfunIotRedboardRp2350Bindings.PWM8_PADS_A),
                            new Rp2350PwmOutput(SparkfunIotRedboardRp2350Bindings.PWM8_OUTPUT_B, SparkfunIotRedboardRp2350Bindings.PWM8_IO_B_CTRL, SparkfunIotRedboardRp2350Bindings.PWM8_PADS_B) });
                case 9:
                    return new Rp2350PwmBinding(SparkfunIotRedboardRp2350Bindings.PWM9_BASE, SparkfunIotRedboardRp2350Bindings.PWM9_RESET_MASK, SparkfunIotRedboardRp2350Bindings.PWM9_SLICE, SparkfunIotRedboardRp2350Bindings.PWM9_FUNCSEL,
                        SparkfunIotRedboardRp2350Bindings.PWM9_CLK_SYS_HZ, new Rp2350PwmOutput[] {
                            new Rp2350PwmOutput(SparkfunIotRedboardRp2350Bindings.PWM9_OUTPUT_A, SparkfunIotRedboardRp2350Bindings.PWM9_IO_A_CTRL, SparkfunIotRedboardRp2350Bindings.PWM9_PADS_A),
                            new Rp2350PwmOutput(SparkfunIotRedboardRp2350Bindings.PWM9_OUTPUT_B, SparkfunIotRedboardRp2350Bindings.PWM9_IO_B_CTRL, SparkfunIotRedboardRp2350Bindings.PWM9_PADS_B) });
                default:
                    throw new ArgumentOutOfRangeException("slice");
            }
        }
#endif
    }
}
