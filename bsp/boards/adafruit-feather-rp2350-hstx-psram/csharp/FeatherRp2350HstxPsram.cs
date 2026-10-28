// Lamella.Boards.Adafruit.FeatherRp2350HstxPsram -- the Adafruit Feather RP2350 with HSTX Port and
// 8MB PSRAM (RP2350A).
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
    internal sealed class FeatherRp2350HstxPsram
    {
        /// <summary>The wire identity this board advertises (lamella_wire::board_model).</summary>
        public static readonly int BoardModel = AdafruitFeatherRp2350HstxPsramBindings.BOARD_MODEL;

        /// <summary>The red LED on GP7, lit by a high level -- the board's blink target.</summary>
        public static readonly int LedPin = (int)AdafruitFeatherRp2350HstxPsramBindings.LED0_PIN;

        /// <summary>The data line of the board's addressable RGB LED, GP21.</summary>
        public static readonly int NeopixelPin = (int)AdafruitFeatherRp2350HstxPsramBindings.NEOPIXEL_PIN;

        /// <summary>The PSRAM's chip select, GP8, as a mask over bank 0. Composed from the generated
        /// mask rather than written out, so a line moved in board.toml moves here.</summary>
        public static readonly uint PsramSelectPins = AdafruitFeatherRp2350HstxPsramBindings.PSRAM_CS_MASK;

        /// <summary>Binds this board's buses to the driver table, so a program writes plain
        /// dotnet/iot -- <c>SpiDevice.Create(settings)</c>, <c>new GpioController()</c> -- and never
        /// names a Lamella type. Touching <see cref="FeatherRp2350HstxPsram"/> at all is what arms it, and
        /// constructing <see cref="TargetBoard"/> touches it.</summary>
        /// <remarks>A type initializer rather than the instance constructor: the table refuses a
        /// second bind of the same id rather than replacing it, and this class is instantiable and
        /// routinely constructed as a temporary. The language runs a type initializer once per
        /// program. The bound values are factories, not drivers, so a program that never touches a
        /// bus never constructs its driver.</remarks>
        static FeatherRp2350HstxPsram()
        {
            Buses.BindSpi(0, new SpiDriverFactory(MakeSpi0));
            Buses.BindI2c(1, new I2cDriverFactory(MakeI2c1));
            Buses.BindGpio(new GpioDriverFactory(MakeGpio));
            AdcControllers.Bind(new AdcDriverFactory(MakeAdc));
#if LAMELLA_SURFACE_FLOAT
            Buses.BindPwm(0, new PwmChannelFactory(OpenPwm0));
            Buses.BindPwm(1, new PwmChannelFactory(OpenPwm1));
            Buses.BindPwm(2, new PwmChannelFactory(OpenPwm2));
            Buses.BindPwm(4, new PwmChannelFactory(OpenPwm4));
            Buses.BindPwm(5, new PwmChannelFactory(OpenPwm5));
            Buses.BindPwm(6, new PwmChannelFactory(OpenPwm6));
            Buses.BindPwm(7, new PwmChannelFactory(OpenPwm7));
#endif
        }

        private static SpiDriver MakeSpi0() { return new Rp2350SpiDriver(SpiBinding(0)); }
        private static I2cDriver MakeI2c1() { return new Rp2350I2cDriver(I2cBinding(1)); }
        private static GpioDriver MakeGpio() { return new Rp2350GpioDriver(PsramSelectPins); }
        private static AdcDriver MakeAdc() { return new Rp2350AdcDriver(AdcBinding()); }

        /// <summary>A GPIO controller over the RP2350 SIO/pad block, bank 0 (GP0..GP29 on this
        /// package). GP8, the PSRAM's chip select, refuses <c>SetPinMode</c>.</summary>
        public GpioController CreateGpioController()
        {
            return new GpioController();
        }

        /// <summary>The on-chip temperature sensor's converter channel.</summary>
        public static readonly int TemperatureSensorChannel = (int)AdafruitFeatherRp2350HstxPsramBindings.ADC_TEMPERATURE_CHANNEL;
        /// <summary>The converter channel that reads GP26, the header's A0.</summary>
        public static readonly int AdcChannelGp26 = (int)AdafruitFeatherRp2350HstxPsramBindings.ADC_GPIO26_CHANNEL;
        /// <summary>The converter channel that reads GP27, the header's A1.</summary>
        public static readonly int AdcChannelGp27 = (int)AdafruitFeatherRp2350HstxPsramBindings.ADC_GPIO27_CHANNEL;
        /// <summary>The converter channel that reads GP28, the header's A2.</summary>
        public static readonly int AdcChannelGp28 = (int)AdafruitFeatherRp2350HstxPsramBindings.ADC_GPIO28_CHANNEL;
        /// <summary>The converter channel that reads GP29, the header's A3.</summary>
        public static readonly int AdcChannelGp29 = (int)AdafruitFeatherRp2350HstxPsramBindings.ADC_GPIO29_CHANNEL;
        /// <summary>The board's converter reference, in microvolts.</summary>
        public static readonly int AdcReferenceMicrovolts = (int)AdafruitFeatherRp2350HstxPsramBindings.ADC_REFERENCE_UV;

        /// <summary>The `adc` binding descriptor, built from the generated constants.</summary>
        public Rp2350AdcBinding CreateAdcBinding() { return AdcBinding(); }

        private static Rp2350AdcBinding AdcBinding()
        {
            return new Rp2350AdcBinding(
                AdafruitFeatherRp2350HstxPsramBindings.ADC_BASE,
                AdafruitFeatherRp2350HstxPsramBindings.ADC_RESET_MASK,
                AdafruitFeatherRp2350HstxPsramBindings.ADC_REFERENCE_UV,
                (int)AdafruitFeatherRp2350HstxPsramBindings.ADC_CHANNEL_COUNT,
                (int)AdafruitFeatherRp2350HstxPsramBindings.ADC_TEMPERATURE_CHANNEL,
                new int[] {
                    (int)AdafruitFeatherRp2350HstxPsramBindings.ADC_CHANNEL0_PIN, (int)AdafruitFeatherRp2350HstxPsramBindings.ADC_CHANNEL1_PIN,
                    (int)AdafruitFeatherRp2350HstxPsramBindings.ADC_CHANNEL2_PIN, (int)AdafruitFeatherRp2350HstxPsramBindings.ADC_CHANNEL3_PIN },
                AdafruitFeatherRp2350HstxPsramBindings.ADC_RESERVED_CHANNELS,
                new string[4],
                new uint[4],
                new uint[4]);
        }

        /// <summary>The on-chip converter as dotnet/iot's analog controller. A pin is opened by its GPIO
        /// number, GP26 to GP29: <c>OpenPin(26)</c> reads GP26, the header's A0, converter channel
        /// <see cref="AdcChannelGp26"/>. The temperature sensor's channel reads no pin, so it does not
        /// open here.</summary>
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
                AdafruitFeatherRp2350HstxPsramBindings.UART0_BASE,
                AdafruitFeatherRp2350HstxPsramBindings.UART0_RESET_MASK,
                AdafruitFeatherRp2350HstxPsramBindings.UART0_IO_TX_CTRL,
                AdafruitFeatherRp2350HstxPsramBindings.UART0_IO_RX_CTRL,
                AdafruitFeatherRp2350HstxPsramBindings.UART0_PADS_TX,
                AdafruitFeatherRp2350HstxPsramBindings.UART0_PADS_RX,
                AdafruitFeatherRp2350HstxPsramBindings.UART0_FUNCSEL,
                AdafruitFeatherRp2350HstxPsramBindings.UART0_CLK_PERI_HZ);
        }

        /// <summary>UART0 on GP0 (TX) / GP1 (RX), the header's TX and RX, ready for
        /// <c>Init(baud)</c>.</summary>
        public Rp2350Uart CreateUart()
        {
            return new Rp2350Uart(CreateUartBinding());
        }

        /// <summary>The `spi0` binding descriptor for <paramref name="busId"/>
        /// (bus 0 = SPI0 on GP20 MISO / GP22 SCK / GP23 MOSI, with no chip select of the board's;
        /// unknown ids refuse loudly).</summary>
        public Rp2350SpiBinding CreateSpiBinding(int busId) { return SpiBinding(busId); }

        private static Rp2350SpiBinding SpiBinding(int busId)
        {
            if (busId != 0)
            {
                throw new ArgumentException("adafruit-feather-rp2350-hstx-psram has no such spi bus: bus 0 = SPI0 on GP20, GP22 and GP23");
            }
            return new Rp2350SpiBinding(
                AdafruitFeatherRp2350HstxPsramBindings.SPI0_BASE,
                AdafruitFeatherRp2350HstxPsramBindings.SPI0_RESET_MASK,
                AdafruitFeatherRp2350HstxPsramBindings.SPI0_IO_MISO_CTRL,
                AdafruitFeatherRp2350HstxPsramBindings.SPI0_PADS_MISO,
                AdafruitFeatherRp2350HstxPsramBindings.SPI0_IO_SCK_CTRL,
                AdafruitFeatherRp2350HstxPsramBindings.SPI0_PADS_SCK,
                AdafruitFeatherRp2350HstxPsramBindings.SPI0_IO_MOSI_CTRL,
                AdafruitFeatherRp2350HstxPsramBindings.SPI0_PADS_MOSI,
                AdafruitFeatherRp2350HstxPsramBindings.SPI0_FUNCSEL,
                AdafruitFeatherRp2350HstxPsramBindings.SPI0_SSPCLK_HZ,
                new int[0]);
        }

        /// <summary>A SPI device per <paramref name="settings"/>: the settings' BusId picks
        /// the descriptor. The bus has no chip select of the board's, so ChipSelectLine is -1 and
        /// the program drives the select pin it chose.</summary>
        public SpiDevice CreateSpiDevice(SpiConnectionSettings settings)
        {
            return SpiDevice.Create(settings);
        }

        /// <summary>A SPI device on bus 0 with <paramref name="chipSelectLine"/>, which is -1 on this
        /// board: the bus has no chip select of the board's.</summary>
        public SpiDevice CreateSpiDevice(int chipSelectLine)
        {
            return CreateSpiDevice(new SpiConnectionSettings(0, chipSelectLine));
        }

        /// <summary>The `i2c1` binding descriptor for <paramref name="busId"/>
        /// (bus 1 = I2C1 on GP2 SDA / GP3 SCL, the header's SDA and SCL and the STEMMA QT
        /// connector; unknown ids refuse loudly).</summary>
        public Rp2350I2cBinding CreateI2cBinding(int busId) { return I2cBinding(busId); }

        private static Rp2350I2cBinding I2cBinding(int busId)
        {
            if (busId != 1)
            {
                throw new ArgumentException("adafruit-feather-rp2350-hstx-psram has no such i2c bus: bus 1 = I2C1 on GP2/GP3");
            }
            return new Rp2350I2cBinding(
                AdafruitFeatherRp2350HstxPsramBindings.I2C1_BASE,
                AdafruitFeatherRp2350HstxPsramBindings.I2C1_RESET_MASK,
                AdafruitFeatherRp2350HstxPsramBindings.I2C1_IO_SDA_CTRL,
                AdafruitFeatherRp2350HstxPsramBindings.I2C1_PADS_SDA,
                AdafruitFeatherRp2350HstxPsramBindings.I2C1_IO_SCL_CTRL,
                AdafruitFeatherRp2350HstxPsramBindings.I2C1_PADS_SCL,
                AdafruitFeatherRp2350HstxPsramBindings.I2C1_FUNCSEL,
                AdafruitFeatherRp2350HstxPsramBindings.I2C1_IC_CLK_HZ);
        }

        /// <summary>An I2C device per <paramref name="settings"/>: the settings' BusId picks
        /// the descriptor.</summary>
        public I2cDevice CreateI2cDevice(I2cConnectionSettings settings)
        {
            return I2cDevice.Create(settings);
        }

        /// <summary>An I2C device on bus 1 (I2C1, GP2/GP3) at <paramref name="deviceAddress"/>.</summary>
        public I2cDevice CreateI2cDevice(int deviceAddress)
        {
            return CreateI2cDevice(new I2cConnectionSettings(1, deviceAddress));
        }

#if LAMELLA_SURFACE_FLOAT
        private static readonly Rp2350PwmSlice[] _pwmSlices = new Rp2350PwmSlice[8];

        private static PwmChannel OpenPwm(int slice, int channel, int frequency, double dutyCyclePercentage)
        {
            if ((object)_pwmSlices[slice] == null)
            {
                _pwmSlices[slice] = new Rp2350PwmSlice(PwmBinding(slice));
            }
            return _pwmSlices[slice].Open(channel, frequency, dutyCyclePercentage);
        }

        private static PwmChannel OpenPwm0(int channel, int frequency, double duty) { return OpenPwm(0, channel, frequency, duty); }
        private static PwmChannel OpenPwm1(int channel, int frequency, double duty) { return OpenPwm(1, channel, frequency, duty); }
        private static PwmChannel OpenPwm2(int channel, int frequency, double duty) { return OpenPwm(2, channel, frequency, duty); }
        private static PwmChannel OpenPwm4(int channel, int frequency, double duty) { return OpenPwm(4, channel, frequency, duty); }
        private static PwmChannel OpenPwm5(int channel, int frequency, double duty) { return OpenPwm(5, channel, frequency, duty); }
        private static PwmChannel OpenPwm6(int channel, int frequency, double duty) { return OpenPwm(6, channel, frequency, duty); }
        private static PwmChannel OpenPwm7(int channel, int frequency, double duty) { return OpenPwm(7, channel, frequency, duty); }

        /// <summary>The binding descriptor of the board's pwm binding for <paramref name="slice"/>: 0, 1,
        /// 2 or 4 to 7. PWM3 is not bound, and any other slice refuses loudly.</summary>
        public Rp2350PwmBinding CreatePwmBinding(int slice) { return PwmBinding(slice); }

        private static Rp2350PwmBinding PwmBinding(int slice)
        {
            switch (slice)
            {
                case 0:
                    return new Rp2350PwmBinding(AdafruitFeatherRp2350HstxPsramBindings.PWM0_BASE, AdafruitFeatherRp2350HstxPsramBindings.PWM0_RESET_MASK, AdafruitFeatherRp2350HstxPsramBindings.PWM0_SLICE, AdafruitFeatherRp2350HstxPsramBindings.PWM0_FUNCSEL,
                        AdafruitFeatherRp2350HstxPsramBindings.PWM0_CLK_SYS_HZ, new Rp2350PwmOutput[] {
                            new Rp2350PwmOutput(AdafruitFeatherRp2350HstxPsramBindings.PWM0_OUTPUT_A, AdafruitFeatherRp2350HstxPsramBindings.PWM0_IO_A_CTRL, AdafruitFeatherRp2350HstxPsramBindings.PWM0_PADS_A),
                            new Rp2350PwmOutput(AdafruitFeatherRp2350HstxPsramBindings.PWM0_OUTPUT_B, AdafruitFeatherRp2350HstxPsramBindings.PWM0_IO_B_CTRL, AdafruitFeatherRp2350HstxPsramBindings.PWM0_PADS_B) });
                case 1:
                    return new Rp2350PwmBinding(AdafruitFeatherRp2350HstxPsramBindings.PWM1_BASE, AdafruitFeatherRp2350HstxPsramBindings.PWM1_RESET_MASK, AdafruitFeatherRp2350HstxPsramBindings.PWM1_SLICE, AdafruitFeatherRp2350HstxPsramBindings.PWM1_FUNCSEL,
                        AdafruitFeatherRp2350HstxPsramBindings.PWM1_CLK_SYS_HZ, new Rp2350PwmOutput[] {
                            new Rp2350PwmOutput(AdafruitFeatherRp2350HstxPsramBindings.PWM1_OUTPUT_A, AdafruitFeatherRp2350HstxPsramBindings.PWM1_IO_A_CTRL, AdafruitFeatherRp2350HstxPsramBindings.PWM1_PADS_A),
                            new Rp2350PwmOutput(AdafruitFeatherRp2350HstxPsramBindings.PWM1_OUTPUT_B, AdafruitFeatherRp2350HstxPsramBindings.PWM1_IO_B_CTRL, AdafruitFeatherRp2350HstxPsramBindings.PWM1_PADS_B) });
                case 2:
                    return new Rp2350PwmBinding(AdafruitFeatherRp2350HstxPsramBindings.PWM2_BASE, AdafruitFeatherRp2350HstxPsramBindings.PWM2_RESET_MASK, AdafruitFeatherRp2350HstxPsramBindings.PWM2_SLICE, AdafruitFeatherRp2350HstxPsramBindings.PWM2_FUNCSEL,
                        AdafruitFeatherRp2350HstxPsramBindings.PWM2_CLK_SYS_HZ, new Rp2350PwmOutput[] {
                            new Rp2350PwmOutput(AdafruitFeatherRp2350HstxPsramBindings.PWM2_OUTPUT_A, AdafruitFeatherRp2350HstxPsramBindings.PWM2_IO_A_CTRL, AdafruitFeatherRp2350HstxPsramBindings.PWM2_PADS_A),
                            new Rp2350PwmOutput(AdafruitFeatherRp2350HstxPsramBindings.PWM2_OUTPUT_B, AdafruitFeatherRp2350HstxPsramBindings.PWM2_IO_B_CTRL, AdafruitFeatherRp2350HstxPsramBindings.PWM2_PADS_B) });
                case 4:
                    return new Rp2350PwmBinding(AdafruitFeatherRp2350HstxPsramBindings.PWM4_BASE, AdafruitFeatherRp2350HstxPsramBindings.PWM4_RESET_MASK, AdafruitFeatherRp2350HstxPsramBindings.PWM4_SLICE, AdafruitFeatherRp2350HstxPsramBindings.PWM4_FUNCSEL,
                        AdafruitFeatherRp2350HstxPsramBindings.PWM4_CLK_SYS_HZ, new Rp2350PwmOutput[] {
                            new Rp2350PwmOutput(AdafruitFeatherRp2350HstxPsramBindings.PWM4_OUTPUT_A, AdafruitFeatherRp2350HstxPsramBindings.PWM4_IO_A_CTRL, AdafruitFeatherRp2350HstxPsramBindings.PWM4_PADS_A),
                            new Rp2350PwmOutput(AdafruitFeatherRp2350HstxPsramBindings.PWM4_OUTPUT_B, AdafruitFeatherRp2350HstxPsramBindings.PWM4_IO_B_CTRL, AdafruitFeatherRp2350HstxPsramBindings.PWM4_PADS_B) });
                case 5:
                    return new Rp2350PwmBinding(AdafruitFeatherRp2350HstxPsramBindings.PWM5_BASE, AdafruitFeatherRp2350HstxPsramBindings.PWM5_RESET_MASK, AdafruitFeatherRp2350HstxPsramBindings.PWM5_SLICE, AdafruitFeatherRp2350HstxPsramBindings.PWM5_FUNCSEL,
                        AdafruitFeatherRp2350HstxPsramBindings.PWM5_CLK_SYS_HZ, new Rp2350PwmOutput[] {
                            new Rp2350PwmOutput(AdafruitFeatherRp2350HstxPsramBindings.PWM5_OUTPUT_A, AdafruitFeatherRp2350HstxPsramBindings.PWM5_IO_A_CTRL, AdafruitFeatherRp2350HstxPsramBindings.PWM5_PADS_A),
                            new Rp2350PwmOutput(AdafruitFeatherRp2350HstxPsramBindings.PWM5_OUTPUT_B, AdafruitFeatherRp2350HstxPsramBindings.PWM5_IO_B_CTRL, AdafruitFeatherRp2350HstxPsramBindings.PWM5_PADS_B) });
                case 6:
                    return new Rp2350PwmBinding(AdafruitFeatherRp2350HstxPsramBindings.PWM6_BASE, AdafruitFeatherRp2350HstxPsramBindings.PWM6_RESET_MASK, AdafruitFeatherRp2350HstxPsramBindings.PWM6_SLICE, AdafruitFeatherRp2350HstxPsramBindings.PWM6_FUNCSEL,
                        AdafruitFeatherRp2350HstxPsramBindings.PWM6_CLK_SYS_HZ, new Rp2350PwmOutput[] {
                            new Rp2350PwmOutput(AdafruitFeatherRp2350HstxPsramBindings.PWM6_OUTPUT_A, AdafruitFeatherRp2350HstxPsramBindings.PWM6_IO_A_CTRL, AdafruitFeatherRp2350HstxPsramBindings.PWM6_PADS_A),
                            new Rp2350PwmOutput(AdafruitFeatherRp2350HstxPsramBindings.PWM6_OUTPUT_B, AdafruitFeatherRp2350HstxPsramBindings.PWM6_IO_B_CTRL, AdafruitFeatherRp2350HstxPsramBindings.PWM6_PADS_B) });
                case 7:
                    return new Rp2350PwmBinding(AdafruitFeatherRp2350HstxPsramBindings.PWM7_BASE, AdafruitFeatherRp2350HstxPsramBindings.PWM7_RESET_MASK, AdafruitFeatherRp2350HstxPsramBindings.PWM7_SLICE, AdafruitFeatherRp2350HstxPsramBindings.PWM7_FUNCSEL,
                        AdafruitFeatherRp2350HstxPsramBindings.PWM7_CLK_SYS_HZ, new Rp2350PwmOutput[] {
                            new Rp2350PwmOutput(AdafruitFeatherRp2350HstxPsramBindings.PWM7_OUTPUT_A, AdafruitFeatherRp2350HstxPsramBindings.PWM7_IO_A_CTRL, AdafruitFeatherRp2350HstxPsramBindings.PWM7_PADS_A),
                            new Rp2350PwmOutput(AdafruitFeatherRp2350HstxPsramBindings.PWM7_OUTPUT_B, AdafruitFeatherRp2350HstxPsramBindings.PWM7_IO_B_CTRL, AdafruitFeatherRp2350HstxPsramBindings.PWM7_PADS_B) });
                default:
                    throw new ArgumentOutOfRangeException("slice");
            }
        }
#endif
    }
}
