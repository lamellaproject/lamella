// Lamella.Boards.Adafruit.MetroRp2350Psram -- the Adafruit Metro RP2350 with PSRAM (RP2350B).
using System;
using System.Device.Analog;
using System.Device.Gpio;
using System.Device.I2c;
using System.Device.Pwm;
using System.Device.Spi;
using Lamella.Generated;
using Lamella.Hardware;

namespace Lamella.Boards.Adafruit
{
    internal sealed class MetroRp2350Psram
    {
        /// <summary>The wire identity this board advertises (lamella_wire::board_model).</summary>
        public static readonly int BoardModel = AdafruitMetroRp2350PsramBindings.BOARD_MODEL;

        /// <summary>The red LED on GP23, the header's 13 position, lit by a high level -- the board's
        /// blink target.</summary>
        public static readonly int LedPin = (int)AdafruitMetroRp2350PsramBindings.LED0_PIN;

        /// <summary>The data line of the board's addressable RGB LED, GP25.</summary>
        public static readonly int NeopixelPin = (int)AdafruitMetroRp2350PsramBindings.NEOPIXEL_PIN;

        /// <summary>Binds this board's buses to the driver table, so a program writes plain
        /// dotnet/iot -- <c>SpiDevice.Create(settings)</c>, <c>new GpioController()</c> -- and never
        /// names a Lamella type. Touching <see cref="MetroRp2350Psram"/> at all is what arms it, and
        /// constructing <see cref="TargetBoard"/> touches it.</summary>
        /// <remarks>A type initializer rather than the instance constructor: the table refuses a
        /// second bind of the same id rather than replacing it, and this class is instantiable and
        /// routinely constructed as a temporary. The language runs a type initializer once per
        /// program. The bound values are factories, not drivers, so a program that never touches a
        /// bus never constructs its driver.</remarks>
        static MetroRp2350Psram()
        {
            Buses.BindSpi(1, new SpiDriverFactory(MakeSpi1));
            Buses.BindI2c(0, new I2cDriverFactory(MakeI2c0));
            Buses.BindGpio(new GpioDriverFactory(MakeGpio));
            AdcControllers.Bind(new AdcDriverFactory(MakeAdc));
#if LAMELLA_SURFACE_FLOAT
            Buses.SetPwmChipCount(AdafruitMetroRp2350PsramBindings.PWM_CHIP_COUNT);
            Buses.BindPwm(0, new PwmChannelFactory(OpenPwm0));
            Buses.BindPwm(1, new PwmChannelFactory(OpenPwm1));
            Buses.BindPwm(2, new PwmChannelFactory(OpenPwm2));
            Buses.BindPwm(3, new PwmChannelFactory(OpenPwm3));
            Buses.BindPwm(4, new PwmChannelFactory(OpenPwm4));
            Buses.BindPwm(5, new PwmChannelFactory(OpenPwm5));
            Buses.BindPwm(6, new PwmChannelFactory(OpenPwm6));
            Buses.BindPwm(7, new PwmChannelFactory(OpenPwm7));
#endif
        }

        private static SpiDriver MakeSpi1() { return new Rp2350SpiDriver(SpiBinding(1)); }
        private static I2cDriver MakeI2c0() { return new Rp2350I2cDriver(I2cBinding(0)); }
        private static GpioDriver MakeGpio()
        {
            return new Rp2350GpioDriver(0u, AdafruitMetroRp2350PsramBindings.PSRAM_CS_BANK1_MASK, new int[0], new int[0]);
        }
        private static AdcDriver MakeAdc() { return new Rp2350AdcDriver(AdcBinding()); }

        /// <summary>A GPIO controller over the RP2350 SIO and pads, GP0 to GP47. GP47, the PSRAM's
        /// chip select, refuses <c>SetPinMode</c>.</summary>
        public GpioController CreateGpioController()
        {
            return new GpioController();
        }

        /// <summary>The on-chip temperature sensor's converter channel.</summary>
        public static readonly int TemperatureSensorChannel = (int)AdafruitMetroRp2350PsramBindings.ADC_TEMPERATURE_CHANNEL;
        /// <summary>The converter channel that reads GP41, the header's A0.</summary>
        public static readonly int AdcChannelGp41 = (int)AdafruitMetroRp2350PsramBindings.ADC_GPIO41_CHANNEL;
        /// <summary>The converter channel that reads GP42, the header's A1.</summary>
        public static readonly int AdcChannelGp42 = (int)AdafruitMetroRp2350PsramBindings.ADC_GPIO42_CHANNEL;
        /// <summary>The converter channel that reads GP43, the header's A2.</summary>
        public static readonly int AdcChannelGp43 = (int)AdafruitMetroRp2350PsramBindings.ADC_GPIO43_CHANNEL;
        /// <summary>The converter channel that reads GP44, the header's A3.</summary>
        public static readonly int AdcChannelGp44 = (int)AdafruitMetroRp2350PsramBindings.ADC_GPIO44_CHANNEL;
        /// <summary>The converter channel that reads GP45, the header's A4.</summary>
        public static readonly int AdcChannelGp45 = (int)AdafruitMetroRp2350PsramBindings.ADC_GPIO45_CHANNEL;
        /// <summary>The converter channel that reads GP46, the header's A5.</summary>
        public static readonly int AdcChannelGp46 = (int)AdafruitMetroRp2350PsramBindings.ADC_GPIO46_CHANNEL;
        /// <summary>The board's converter reference, in microvolts.</summary>
        public static readonly int AdcReferenceMicrovolts = (int)AdafruitMetroRp2350PsramBindings.ADC_REFERENCE_UV;

        /// <summary>The `adc` binding descriptor, built from the generated constants.</summary>
        public Rp2350AdcBinding CreateAdcBinding() { return AdcBinding(); }

        private static Rp2350AdcBinding AdcBinding()
        {
            return new Rp2350AdcBinding(
                AdafruitMetroRp2350PsramBindings.ADC_BASE,
                AdafruitMetroRp2350PsramBindings.ADC_RESET_MASK,
                AdafruitMetroRp2350PsramBindings.ADC_REFERENCE_UV,
                (int)AdafruitMetroRp2350PsramBindings.ADC_CHANNEL_COUNT,
                (int)AdafruitMetroRp2350PsramBindings.ADC_TEMPERATURE_CHANNEL,
                new int[] {
                    (int)AdafruitMetroRp2350PsramBindings.ADC_CHANNEL0_PIN, (int)AdafruitMetroRp2350PsramBindings.ADC_CHANNEL1_PIN,
                    (int)AdafruitMetroRp2350PsramBindings.ADC_CHANNEL2_PIN, (int)AdafruitMetroRp2350PsramBindings.ADC_CHANNEL3_PIN,
                    (int)AdafruitMetroRp2350PsramBindings.ADC_CHANNEL4_PIN, (int)AdafruitMetroRp2350PsramBindings.ADC_CHANNEL5_PIN,
                    (int)AdafruitMetroRp2350PsramBindings.ADC_CHANNEL6_PIN, (int)AdafruitMetroRp2350PsramBindings.ADC_CHANNEL7_PIN },
                AdafruitMetroRp2350PsramBindings.ADC_RESERVED_CHANNELS,
                new string[] {
                    AdafruitMetroRp2350PsramBindings.ADC_CHANNEL0_RESERVED_BY, null, null, null, null, null, null,
                    AdafruitMetroRp2350PsramBindings.ADC_CHANNEL7_RESERVED_BY },
                new uint[8],
                new uint[8]);
        }

        /// <summary>The on-chip converter as dotnet/iot's analog controller. A pin is opened by its GPIO
        /// number, GP41 to GP46: <c>OpenPin(41)</c> reads GP41, the header's A0, converter channel
        /// <see cref="AdcChannelGp41"/>. GP40 and GP47 are the board's own lines, and the temperature
        /// sensor's channel reads no pin, so none of the three opens here.</summary>
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
                AdafruitMetroRp2350PsramBindings.UART0_BASE,
                AdafruitMetroRp2350PsramBindings.UART0_RESET_MASK,
                AdafruitMetroRp2350PsramBindings.UART0_IO_TX_CTRL,
                AdafruitMetroRp2350PsramBindings.UART0_IO_RX_CTRL,
                AdafruitMetroRp2350PsramBindings.UART0_PADS_TX,
                AdafruitMetroRp2350PsramBindings.UART0_PADS_RX,
                AdafruitMetroRp2350PsramBindings.UART0_FUNCSEL,
                AdafruitMetroRp2350PsramBindings.UART0_CLK_PERI_HZ);
        }

        /// <summary>UART0 on GP0 (TX) / GP1 (RX), which reach the header's RX and TX positions through
        /// the board's RX/TX switch, ready for <c>Init(baud)</c>.</summary>
        public Rp2350Uart CreateUart()
        {
            return new Rp2350Uart(CreateUartBinding());
        }

        /// <summary>The `spi1` binding descriptor for <paramref name="busId"/>
        /// (bus 1 = SPI1 on GP28 MISO / GP30 SCK / GP31 MOSI, the ICSP header, with no chip select of
        /// the board's; unknown ids refuse loudly).</summary>
        public Rp2350SpiBinding CreateSpiBinding(int busId) { return SpiBinding(busId); }

        private static Rp2350SpiBinding SpiBinding(int busId)
        {
            if (busId != 1)
            {
                throw new ArgumentException("adafruit-metro-rp2350-psram has no such spi bus: bus 1 = SPI1 on GP28, GP30 and GP31");
            }
            return new Rp2350SpiBinding(
                AdafruitMetroRp2350PsramBindings.SPI1_BASE,
                AdafruitMetroRp2350PsramBindings.SPI1_RESET_MASK,
                AdafruitMetroRp2350PsramBindings.SPI1_IO_MISO_CTRL,
                AdafruitMetroRp2350PsramBindings.SPI1_PADS_MISO,
                AdafruitMetroRp2350PsramBindings.SPI1_IO_SCK_CTRL,
                AdafruitMetroRp2350PsramBindings.SPI1_PADS_SCK,
                AdafruitMetroRp2350PsramBindings.SPI1_IO_MOSI_CTRL,
                AdafruitMetroRp2350PsramBindings.SPI1_PADS_MOSI,
                AdafruitMetroRp2350PsramBindings.SPI1_FUNCSEL,
                AdafruitMetroRp2350PsramBindings.SPI1_SSPCLK_HZ,
                new int[0]);
        }

        /// <summary>A SPI device per <paramref name="settings"/>: the settings' BusId picks
        /// the descriptor. The bus has no chip select of the board's, so ChipSelectLine is -1 and
        /// the program drives the select pin it chose.</summary>
        public SpiDevice CreateSpiDevice(SpiConnectionSettings settings)
        {
            return SpiDevice.Create(settings);
        }

        /// <summary>A SPI device on bus 1 with <paramref name="chipSelectLine"/>, which is -1 on this
        /// board: the bus has no chip select of the board's.</summary>
        public SpiDevice CreateSpiDevice(int chipSelectLine)
        {
            return CreateSpiDevice(new SpiConnectionSettings(1, chipSelectLine));
        }

        /// <summary>The `i2c0` binding descriptor for <paramref name="busId"/>
        /// (bus 0 = I2C0 on GP20 SDA / GP21 SCL, the header's SDA and SCL and the STEMMA QT
        /// connector; unknown ids refuse loudly).</summary>
        public Rp2350I2cBinding CreateI2cBinding(int busId) { return I2cBinding(busId); }

        private static Rp2350I2cBinding I2cBinding(int busId)
        {
            if (busId != 0)
            {
                throw new ArgumentException("adafruit-metro-rp2350-psram has no such i2c bus: bus 0 = I2C0 on GP20/GP21");
            }
            return new Rp2350I2cBinding(
                AdafruitMetroRp2350PsramBindings.I2C0_BASE,
                AdafruitMetroRp2350PsramBindings.I2C0_RESET_MASK,
                AdafruitMetroRp2350PsramBindings.I2C0_IO_SDA_CTRL,
                AdafruitMetroRp2350PsramBindings.I2C0_PADS_SDA,
                AdafruitMetroRp2350PsramBindings.I2C0_IO_SCL_CTRL,
                AdafruitMetroRp2350PsramBindings.I2C0_PADS_SCL,
                AdafruitMetroRp2350PsramBindings.I2C0_FUNCSEL,
                AdafruitMetroRp2350PsramBindings.I2C0_IC_CLK_HZ);
        }

        /// <summary>An I2C device per <paramref name="settings"/>: the settings' BusId picks
        /// the descriptor.</summary>
        public I2cDevice CreateI2cDevice(I2cConnectionSettings settings)
        {
            return I2cDevice.Create(settings);
        }

        /// <summary>An I2C device on bus 0 (I2C0, GP20/GP21) at <paramref name="deviceAddress"/>.</summary>
        public I2cDevice CreateI2cDevice(int deviceAddress)
        {
            return CreateI2cDevice(new I2cConnectionSettings(0, deviceAddress));
        }

#if LAMELLA_SURFACE_FLOAT
        private static readonly Rp2350PwmSlice[] _pwmSlices = new Rp2350PwmSlice[8];

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
        private static PwmChannel OpenPwm2(int channel, int frequency, double duty) { return OpenPwm(2, channel, frequency, duty); }
        private static PwmChannel OpenPwm3(int channel, int frequency, double duty) { return OpenPwm(3, channel, frequency, duty); }
        private static PwmChannel OpenPwm4(int channel, int frequency, double duty) { return OpenPwm(4, channel, frequency, duty); }
        private static PwmChannel OpenPwm5(int channel, int frequency, double duty) { return OpenPwm(5, channel, frequency, duty); }
        private static PwmChannel OpenPwm6(int channel, int frequency, double duty) { return OpenPwm(6, channel, frequency, duty); }
        private static PwmChannel OpenPwm7(int channel, int frequency, double duty) { return OpenPwm(7, channel, frequency, duty); }

        /// <summary>The binding descriptor of the board's pwm binding for <paramref name="slice"/>, 0 to
        /// 7. Any other slice refuses loudly.</summary>
        public Rp2350PwmBinding CreatePwmBinding(int slice) { return PwmBinding(slice); }

        private static Rp2350PwmBinding PwmBinding(int slice)
        {
            switch (slice)
            {
                case 0:
                    return new Rp2350PwmBinding(AdafruitMetroRp2350PsramBindings.PWM0_BASE, AdafruitMetroRp2350PsramBindings.PWM0_RESET_MASK, AdafruitMetroRp2350PsramBindings.PWM0_SLICE, AdafruitMetroRp2350PsramBindings.PWM0_FUNCSEL,
                        AdafruitMetroRp2350PsramBindings.PWM0_CLK_SYS_HZ, new Rp2350PwmOutput[] {
                            new Rp2350PwmOutput(AdafruitMetroRp2350PsramBindings.PWM0_OUTPUT_A, AdafruitMetroRp2350PsramBindings.PWM0_IO_A_CTRL, AdafruitMetroRp2350PsramBindings.PWM0_PADS_A),
                            new Rp2350PwmOutput(AdafruitMetroRp2350PsramBindings.PWM0_OUTPUT_B, AdafruitMetroRp2350PsramBindings.PWM0_IO_B_CTRL, AdafruitMetroRp2350PsramBindings.PWM0_PADS_B) });
                case 1:
                    return new Rp2350PwmBinding(AdafruitMetroRp2350PsramBindings.PWM1_BASE, AdafruitMetroRp2350PsramBindings.PWM1_RESET_MASK, AdafruitMetroRp2350PsramBindings.PWM1_SLICE, AdafruitMetroRp2350PsramBindings.PWM1_FUNCSEL,
                        AdafruitMetroRp2350PsramBindings.PWM1_CLK_SYS_HZ, new Rp2350PwmOutput[] {
                            new Rp2350PwmOutput(AdafruitMetroRp2350PsramBindings.PWM1_OUTPUT_A, AdafruitMetroRp2350PsramBindings.PWM1_IO_A_CTRL, AdafruitMetroRp2350PsramBindings.PWM1_PADS_A),
                            new Rp2350PwmOutput(AdafruitMetroRp2350PsramBindings.PWM1_OUTPUT_B, AdafruitMetroRp2350PsramBindings.PWM1_IO_B_CTRL, AdafruitMetroRp2350PsramBindings.PWM1_PADS_B) });
                case 2:
                    return new Rp2350PwmBinding(AdafruitMetroRp2350PsramBindings.PWM2_BASE, AdafruitMetroRp2350PsramBindings.PWM2_RESET_MASK, AdafruitMetroRp2350PsramBindings.PWM2_SLICE, AdafruitMetroRp2350PsramBindings.PWM2_FUNCSEL,
                        AdafruitMetroRp2350PsramBindings.PWM2_CLK_SYS_HZ, new Rp2350PwmOutput[] {
                            new Rp2350PwmOutput(AdafruitMetroRp2350PsramBindings.PWM2_OUTPUT_A, AdafruitMetroRp2350PsramBindings.PWM2_IO_A_CTRL, AdafruitMetroRp2350PsramBindings.PWM2_PADS_A),
                            new Rp2350PwmOutput(AdafruitMetroRp2350PsramBindings.PWM2_OUTPUT_B, AdafruitMetroRp2350PsramBindings.PWM2_IO_B_CTRL, AdafruitMetroRp2350PsramBindings.PWM2_PADS_B) });
                case 3:
                    return new Rp2350PwmBinding(AdafruitMetroRp2350PsramBindings.PWM3_BASE, AdafruitMetroRp2350PsramBindings.PWM3_RESET_MASK, AdafruitMetroRp2350PsramBindings.PWM3_SLICE, AdafruitMetroRp2350PsramBindings.PWM3_FUNCSEL,
                        AdafruitMetroRp2350PsramBindings.PWM3_CLK_SYS_HZ, new Rp2350PwmOutput[] {
                            new Rp2350PwmOutput(AdafruitMetroRp2350PsramBindings.PWM3_OUTPUT_A, AdafruitMetroRp2350PsramBindings.PWM3_IO_A_CTRL, AdafruitMetroRp2350PsramBindings.PWM3_PADS_A),
                            new Rp2350PwmOutput(AdafruitMetroRp2350PsramBindings.PWM3_OUTPUT_B, AdafruitMetroRp2350PsramBindings.PWM3_IO_B_CTRL, AdafruitMetroRp2350PsramBindings.PWM3_PADS_B) });
                case 4:
                    return new Rp2350PwmBinding(AdafruitMetroRp2350PsramBindings.PWM4_BASE, AdafruitMetroRp2350PsramBindings.PWM4_RESET_MASK, AdafruitMetroRp2350PsramBindings.PWM4_SLICE, AdafruitMetroRp2350PsramBindings.PWM4_FUNCSEL,
                        AdafruitMetroRp2350PsramBindings.PWM4_CLK_SYS_HZ, new Rp2350PwmOutput[] {
                            new Rp2350PwmOutput(AdafruitMetroRp2350PsramBindings.PWM4_OUTPUT_A, AdafruitMetroRp2350PsramBindings.PWM4_IO_A_CTRL, AdafruitMetroRp2350PsramBindings.PWM4_PADS_A),
                            new Rp2350PwmOutput(AdafruitMetroRp2350PsramBindings.PWM4_OUTPUT_B, AdafruitMetroRp2350PsramBindings.PWM4_IO_B_CTRL, AdafruitMetroRp2350PsramBindings.PWM4_PADS_B) });
                case 5:
                    return new Rp2350PwmBinding(AdafruitMetroRp2350PsramBindings.PWM5_BASE, AdafruitMetroRp2350PsramBindings.PWM5_RESET_MASK, AdafruitMetroRp2350PsramBindings.PWM5_SLICE, AdafruitMetroRp2350PsramBindings.PWM5_FUNCSEL,
                        AdafruitMetroRp2350PsramBindings.PWM5_CLK_SYS_HZ, new Rp2350PwmOutput[] {
                            new Rp2350PwmOutput(AdafruitMetroRp2350PsramBindings.PWM5_OUTPUT_A, AdafruitMetroRp2350PsramBindings.PWM5_IO_A_CTRL, AdafruitMetroRp2350PsramBindings.PWM5_PADS_A),
                            new Rp2350PwmOutput(AdafruitMetroRp2350PsramBindings.PWM5_OUTPUT_B, AdafruitMetroRp2350PsramBindings.PWM5_IO_B_CTRL, AdafruitMetroRp2350PsramBindings.PWM5_PADS_B) });
                case 6:
                    return new Rp2350PwmBinding(AdafruitMetroRp2350PsramBindings.PWM6_BASE, AdafruitMetroRp2350PsramBindings.PWM6_RESET_MASK, AdafruitMetroRp2350PsramBindings.PWM6_SLICE, AdafruitMetroRp2350PsramBindings.PWM6_FUNCSEL,
                        AdafruitMetroRp2350PsramBindings.PWM6_CLK_SYS_HZ, new Rp2350PwmOutput[] {
                            new Rp2350PwmOutput(AdafruitMetroRp2350PsramBindings.PWM6_OUTPUT_A, AdafruitMetroRp2350PsramBindings.PWM6_IO_A_CTRL, AdafruitMetroRp2350PsramBindings.PWM6_PADS_A),
                            new Rp2350PwmOutput(AdafruitMetroRp2350PsramBindings.PWM6_OUTPUT_B, AdafruitMetroRp2350PsramBindings.PWM6_IO_B_CTRL, AdafruitMetroRp2350PsramBindings.PWM6_PADS_B) });
                case 7:
                    return new Rp2350PwmBinding(AdafruitMetroRp2350PsramBindings.PWM7_BASE, AdafruitMetroRp2350PsramBindings.PWM7_RESET_MASK, AdafruitMetroRp2350PsramBindings.PWM7_SLICE, AdafruitMetroRp2350PsramBindings.PWM7_FUNCSEL,
                        AdafruitMetroRp2350PsramBindings.PWM7_CLK_SYS_HZ, new Rp2350PwmOutput[] {
                            new Rp2350PwmOutput(AdafruitMetroRp2350PsramBindings.PWM7_OUTPUT_A, AdafruitMetroRp2350PsramBindings.PWM7_IO_A_CTRL, AdafruitMetroRp2350PsramBindings.PWM7_PADS_A),
                            new Rp2350PwmOutput(AdafruitMetroRp2350PsramBindings.PWM7_OUTPUT_B, AdafruitMetroRp2350PsramBindings.PWM7_IO_B_CTRL, AdafruitMetroRp2350PsramBindings.PWM7_PADS_B) });
                default:
                    throw new ArgumentOutOfRangeException("slice");
            }
        }
#endif
    }
}
