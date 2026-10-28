// Lamella.Boards.Pimoroni.PicoLipo2 -- the Pimoroni Pico LiPo 2 (RP2350B).
using System;
using System.Device.Analog;
using System.Device.Gpio;
using System.Device.I2c;
using System.Device.Pwm;
using System.Device.Spi;
using Lamella.Generated;
using Lamella.Hardware;

namespace Lamella.Boards.Pimoroni
{
    internal sealed class PicoLipo2
    {
        /// <summary>The wire identity this board advertises (lamella_wire::board_model).</summary>
        public static readonly int BoardModel = PimoroniPicoLipo2Bindings.BOARD_MODEL;

        /// <summary>The user LED on GP25, lit by a high level -- the board's blink target.</summary>
        public static readonly int LedPin = (int)PimoroniPicoLipo2Bindings.LED0_PIN;

        /// <summary>Binds this board's buses to the driver table, so a program writes plain
        /// dotnet/iot -- <c>SpiDevice.Create(settings)</c>, <c>new GpioController()</c> -- and never
        /// names a Lamella type. Touching <see cref="PicoLipo2"/> at all is what arms it, and constructing
        /// <see cref="TargetBoard"/> touches it.</summary>
        /// <remarks>A type initializer rather than the instance constructor: the table refuses a
        /// second bind of the same id rather than replacing it, and this class is instantiable and
        /// routinely constructed as a temporary. The language runs a type initializer once per
        /// program. The bound values are factories, not drivers, so a program that never touches a
        /// bus never constructs its driver.</remarks>
        static PicoLipo2()
        {
            Buses.BindSpi(0, new SpiDriverFactory(MakeSpi0));
            Buses.BindI2c(0, new I2cDriverFactory(MakeI2c0));
            Buses.BindGpio(new GpioDriverFactory(MakeGpio));
            AdcControllers.Bind(new AdcDriverFactory(MakeAdc));
#if LAMELLA_SURFACE_FLOAT
            Buses.BindPwm(1, new PwmChannelFactory(OpenPwm1));
            Buses.BindPwm(2, new PwmChannelFactory(OpenPwm2));
            Buses.BindPwm(3, new PwmChannelFactory(OpenPwm3));
            Buses.BindPwm(4, new PwmChannelFactory(OpenPwm4));
            Buses.BindPwm(5, new PwmChannelFactory(OpenPwm5));
            Buses.BindPwm(6, new PwmChannelFactory(OpenPwm6));
            Buses.BindPwm(7, new PwmChannelFactory(OpenPwm7));
#endif
        }

        private static SpiDriver MakeSpi0() { return new Rp2350SpiDriver(SpiBinding(0)); }
        private static I2cDriver MakeI2c0() { return new Rp2350I2cDriver(I2cBinding(0)); }
        private static GpioDriver MakeGpio()
        {
            return new Rp2350GpioDriver(0u,
                new int[] {
                    (int)PimoroniPicoLipo2Bindings.ADC_CHANNEL0_TWIN_PIN,
                    (int)PimoroniPicoLipo2Bindings.ADC_CHANNEL1_TWIN_PIN,
                    (int)PimoroniPicoLipo2Bindings.ADC_CHANNEL2_TWIN_PIN },
                new int[] {
                    (int)PimoroniPicoLipo2Bindings.ADC_CHANNEL0_PIN,
                    (int)PimoroniPicoLipo2Bindings.ADC_CHANNEL1_PIN,
                    (int)PimoroniPicoLipo2Bindings.ADC_CHANNEL2_PIN });
        }
        private static AdcDriver MakeAdc() { return new Rp2350AdcDriver(AdcBinding()); }

        /// <summary>A GPIO controller over the RP2350 SIO/pad block, bank 0 (GP0..GP31). A mode set
        /// on GP26, GP27 or GP28 leaves the converter's pad on the same header position (GP40, GP41
        /// or GP42) high-impedance, with no pull.</summary>
        public GpioController CreateGpioController()
        {
            return new GpioController();
        }

        /// <summary>The on-chip temperature sensor's converter channel.</summary>
        public static readonly int TemperatureSensorChannel = (int)PimoroniPicoLipo2Bindings.ADC_TEMPERATURE_CHANNEL;
        /// <summary>The converter channel that reads GP40, which header position 26 (26 / A0)
        /// reaches through 1k.</summary>
        public static readonly int AdcChannelGp40 = (int)PimoroniPicoLipo2Bindings.ADC_GPIO40_CHANNEL;
        /// <summary>The converter channel that reads GP41, which header position 27 (27 / A1)
        /// reaches through 1k.</summary>
        public static readonly int AdcChannelGp41 = (int)PimoroniPicoLipo2Bindings.ADC_GPIO41_CHANNEL;
        /// <summary>The converter channel that reads GP42, which header position 28 (28 / A2)
        /// reaches through 1k.</summary>
        public static readonly int AdcChannelGp42 = (int)PimoroniPicoLipo2Bindings.ADC_GPIO42_CHANNEL;
        /// <summary>The converter channel that reads GP43, the board's battery sense: a third of the
        /// battery's voltage while the 3.3 V rail is up.</summary>
        public static readonly int AdcChannelGp43 = (int)PimoroniPicoLipo2Bindings.ADC_GPIO43_CHANNEL;
        /// <summary>The board's converter reference, in microvolts.</summary>
        public static readonly int AdcReferenceMicrovolts = (int)PimoroniPicoLipo2Bindings.ADC_REFERENCE_UV;

        /// <summary>The `adc` binding descriptor, built from the generated constants.</summary>
        public Rp2350AdcBinding CreateAdcBinding() { return AdcBinding(); }

        private static Rp2350AdcBinding AdcBinding()
        {
            return new Rp2350AdcBinding(
                PimoroniPicoLipo2Bindings.ADC_BASE,
                PimoroniPicoLipo2Bindings.ADC_RESET_MASK,
                PimoroniPicoLipo2Bindings.ADC_REFERENCE_UV,
                (int)PimoroniPicoLipo2Bindings.ADC_CHANNEL_COUNT,
                (int)PimoroniPicoLipo2Bindings.ADC_TEMPERATURE_CHANNEL,
                new int[] {
                    (int)PimoroniPicoLipo2Bindings.ADC_CHANNEL0_PIN, (int)PimoroniPicoLipo2Bindings.ADC_CHANNEL1_PIN,
                    (int)PimoroniPicoLipo2Bindings.ADC_CHANNEL2_PIN, (int)PimoroniPicoLipo2Bindings.ADC_CHANNEL3_PIN,
                    (int)PimoroniPicoLipo2Bindings.ADC_CHANNEL4_PIN, (int)PimoroniPicoLipo2Bindings.ADC_CHANNEL5_PIN,
                    (int)PimoroniPicoLipo2Bindings.ADC_CHANNEL6_PIN, (int)PimoroniPicoLipo2Bindings.ADC_CHANNEL7_PIN },
                PimoroniPicoLipo2Bindings.ADC_RESERVED_CHANNELS,
                new string[] {
                    null, null, null, null, null, PimoroniPicoLipo2Bindings.ADC_CHANNEL5_RESERVED_BY, null,
                    PimoroniPicoLipo2Bindings.ADC_CHANNEL7_RESERVED_BY },
                new uint[] {
                    PimoroniPicoLipo2Bindings.ADC_CHANNEL0_TWIN_IO_CTRL, PimoroniPicoLipo2Bindings.ADC_CHANNEL1_TWIN_IO_CTRL,
                    PimoroniPicoLipo2Bindings.ADC_CHANNEL2_TWIN_IO_CTRL, 0u, 0u, 0u, 0u, 0u },
                new uint[] {
                    PimoroniPicoLipo2Bindings.ADC_CHANNEL0_TWIN_PADS, PimoroniPicoLipo2Bindings.ADC_CHANNEL1_TWIN_PADS,
                    PimoroniPicoLipo2Bindings.ADC_CHANNEL2_TWIN_PADS, 0u, 0u, 0u, 0u, 0u });
        }

        /// <summary>The on-chip converter as dotnet/iot's analog controller. A pin is opened by its GPIO
        /// number, GP40 to GP47: <c>OpenPin(40)</c> reads GP40, converter channel
        /// <see cref="AdcChannelGp40"/>, which header position 26 reaches through 1k. The temperature
        /// sensor's channel reads no pin, so it does not open here.</summary>
        /// <remarks>Creating it touches no hardware; the first pin opened brings the converter up.
        /// It reads through the converter driver the board binds.
        /// GP45 and GP47 are the button and the PSRAM chip select and refuse to open.</remarks>
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
                PimoroniPicoLipo2Bindings.UART0_BASE,
                PimoroniPicoLipo2Bindings.UART0_RESET_MASK,
                PimoroniPicoLipo2Bindings.UART0_IO_TX_CTRL,
                PimoroniPicoLipo2Bindings.UART0_IO_RX_CTRL,
                PimoroniPicoLipo2Bindings.UART0_PADS_TX,
                PimoroniPicoLipo2Bindings.UART0_PADS_RX,
                PimoroniPicoLipo2Bindings.UART0_FUNCSEL,
                PimoroniPicoLipo2Bindings.UART0_CLK_PERI_HZ);
        }

        /// <summary>UART0 on GP0 (TX, header pin 1) / GP1 (RX, pin 2), ready for
        /// <c>Init(baud)</c>.</summary>
        public Rp2350Uart CreateUart()
        {
            return new Rp2350Uart(CreateUartBinding());
        }

        /// <summary>The `spi0` binding descriptor for <paramref name="busId"/>
        /// (bus 0 = SPI0 on GP16 MISO / GP18 SCK / GP19 MOSI, whose chip select 0 is GP17;
        /// unknown ids refuse loudly).</summary>
        public Rp2350SpiBinding CreateSpiBinding(int busId) { return SpiBinding(busId); }

        private static Rp2350SpiBinding SpiBinding(int busId)
        {
            if (busId != 0)
            {
                throw new ArgumentException("pimoroni-pico-lipo-2 has no such spi bus: bus 0 = SPI0 on GP16..GP19");
            }
            return new Rp2350SpiBinding(
                PimoroniPicoLipo2Bindings.SPI0_BASE,
                PimoroniPicoLipo2Bindings.SPI0_RESET_MASK,
                PimoroniPicoLipo2Bindings.SPI0_IO_MISO_CTRL,
                PimoroniPicoLipo2Bindings.SPI0_PADS_MISO,
                PimoroniPicoLipo2Bindings.SPI0_IO_SCK_CTRL,
                PimoroniPicoLipo2Bindings.SPI0_PADS_SCK,
                PimoroniPicoLipo2Bindings.SPI0_IO_MOSI_CTRL,
                PimoroniPicoLipo2Bindings.SPI0_PADS_MOSI,
                PimoroniPicoLipo2Bindings.SPI0_FUNCSEL,
                PimoroniPicoLipo2Bindings.SPI0_SSPCLK_HZ,
                new int[] { (int)PimoroniPicoLipo2Bindings.SPI0_CHIP_SELECT0 });
        }

        /// <summary>A SPI device per <paramref name="settings"/>: the settings' BusId picks
        /// the descriptor. ChipSelectLine is an index into the bus's chip selects: 0 is GP17 on
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
        /// (bus 0 = I2C0 on GP4 SDA / GP5 SCL, also the Qw/ST connector; unknown ids refuse
        /// loudly).</summary>
        public Rp2350I2cBinding CreateI2cBinding(int busId) { return I2cBinding(busId); }

        private static Rp2350I2cBinding I2cBinding(int busId)
        {
            if (busId != 0)
            {
                throw new ArgumentException("pimoroni-pico-lipo-2 has no such i2c bus: bus 0 = I2C0 on GP4/GP5");
            }
            return new Rp2350I2cBinding(
                PimoroniPicoLipo2Bindings.I2C0_BASE,
                PimoroniPicoLipo2Bindings.I2C0_RESET_MASK,
                PimoroniPicoLipo2Bindings.I2C0_IO_SDA_CTRL,
                PimoroniPicoLipo2Bindings.I2C0_PADS_SDA,
                PimoroniPicoLipo2Bindings.I2C0_IO_SCL_CTRL,
                PimoroniPicoLipo2Bindings.I2C0_PADS_SCL,
                PimoroniPicoLipo2Bindings.I2C0_FUNCSEL,
                PimoroniPicoLipo2Bindings.I2C0_IC_CLK_HZ);
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
        private static readonly Rp2350PwmSlice[] _pwmSlices = new Rp2350PwmSlice[8];

        private static PwmChannel OpenPwm(int slice, int channel, int frequency, double dutyCyclePercentage)
        {
            if ((object)_pwmSlices[slice] == null)
            {
                _pwmSlices[slice] = new Rp2350PwmSlice(PwmBinding(slice));
            }
            return _pwmSlices[slice].Open(channel, frequency, dutyCyclePercentage);
        }

        private static PwmChannel OpenPwm1(int channel, int frequency, double duty) { return OpenPwm(1, channel, frequency, duty); }
        private static PwmChannel OpenPwm2(int channel, int frequency, double duty) { return OpenPwm(2, channel, frequency, duty); }
        private static PwmChannel OpenPwm3(int channel, int frequency, double duty) { return OpenPwm(3, channel, frequency, duty); }
        private static PwmChannel OpenPwm4(int channel, int frequency, double duty) { return OpenPwm(4, channel, frequency, duty); }
        private static PwmChannel OpenPwm5(int channel, int frequency, double duty) { return OpenPwm(5, channel, frequency, duty); }
        private static PwmChannel OpenPwm6(int channel, int frequency, double duty) { return OpenPwm(6, channel, frequency, duty); }
        private static PwmChannel OpenPwm7(int channel, int frequency, double duty) { return OpenPwm(7, channel, frequency, duty); }

        /// <summary>The binding descriptor of the board's pwm binding for <paramref name="slice"/>, from
        /// 1 to 7. The first slice is not bound, and any slice outside 1 to 7 refuses loudly.</summary>
        public Rp2350PwmBinding CreatePwmBinding(int slice) { return PwmBinding(slice); }

        private static Rp2350PwmBinding PwmBinding(int slice)
        {
            switch (slice)
            {
                case 1:
                    return new Rp2350PwmBinding(PimoroniPicoLipo2Bindings.PWM1_BASE, PimoroniPicoLipo2Bindings.PWM1_RESET_MASK, PimoroniPicoLipo2Bindings.PWM1_SLICE, PimoroniPicoLipo2Bindings.PWM1_FUNCSEL,
                        PimoroniPicoLipo2Bindings.PWM1_CLK_SYS_HZ, new Rp2350PwmOutput[] {
                            new Rp2350PwmOutput(PimoroniPicoLipo2Bindings.PWM1_OUTPUT_A, PimoroniPicoLipo2Bindings.PWM1_IO_A_CTRL, PimoroniPicoLipo2Bindings.PWM1_PADS_A),
                            new Rp2350PwmOutput(PimoroniPicoLipo2Bindings.PWM1_OUTPUT_B, PimoroniPicoLipo2Bindings.PWM1_IO_B_CTRL, PimoroniPicoLipo2Bindings.PWM1_PADS_B) });
                case 2:
                    return new Rp2350PwmBinding(PimoroniPicoLipo2Bindings.PWM2_BASE, PimoroniPicoLipo2Bindings.PWM2_RESET_MASK, PimoroniPicoLipo2Bindings.PWM2_SLICE, PimoroniPicoLipo2Bindings.PWM2_FUNCSEL,
                        PimoroniPicoLipo2Bindings.PWM2_CLK_SYS_HZ, new Rp2350PwmOutput[] {
                            new Rp2350PwmOutput(PimoroniPicoLipo2Bindings.PWM2_OUTPUT_A, PimoroniPicoLipo2Bindings.PWM2_IO_A_CTRL, PimoroniPicoLipo2Bindings.PWM2_PADS_A),
                            new Rp2350PwmOutput(PimoroniPicoLipo2Bindings.PWM2_OUTPUT_B, PimoroniPicoLipo2Bindings.PWM2_IO_B_CTRL, PimoroniPicoLipo2Bindings.PWM2_PADS_B) });
                case 3:
                    return new Rp2350PwmBinding(PimoroniPicoLipo2Bindings.PWM3_BASE, PimoroniPicoLipo2Bindings.PWM3_RESET_MASK, PimoroniPicoLipo2Bindings.PWM3_SLICE, PimoroniPicoLipo2Bindings.PWM3_FUNCSEL,
                        PimoroniPicoLipo2Bindings.PWM3_CLK_SYS_HZ, new Rp2350PwmOutput[] {
                            new Rp2350PwmOutput(PimoroniPicoLipo2Bindings.PWM3_OUTPUT_A, PimoroniPicoLipo2Bindings.PWM3_IO_A_CTRL, PimoroniPicoLipo2Bindings.PWM3_PADS_A),
                            new Rp2350PwmOutput(PimoroniPicoLipo2Bindings.PWM3_OUTPUT_B, PimoroniPicoLipo2Bindings.PWM3_IO_B_CTRL, PimoroniPicoLipo2Bindings.PWM3_PADS_B) });
                case 4:
                    return new Rp2350PwmBinding(PimoroniPicoLipo2Bindings.PWM4_BASE, PimoroniPicoLipo2Bindings.PWM4_RESET_MASK, PimoroniPicoLipo2Bindings.PWM4_SLICE, PimoroniPicoLipo2Bindings.PWM4_FUNCSEL,
                        PimoroniPicoLipo2Bindings.PWM4_CLK_SYS_HZ, new Rp2350PwmOutput[] {
                            new Rp2350PwmOutput(PimoroniPicoLipo2Bindings.PWM4_OUTPUT_A, PimoroniPicoLipo2Bindings.PWM4_IO_A_CTRL, PimoroniPicoLipo2Bindings.PWM4_PADS_A),
                            new Rp2350PwmOutput(PimoroniPicoLipo2Bindings.PWM4_OUTPUT_B, PimoroniPicoLipo2Bindings.PWM4_IO_B_CTRL, PimoroniPicoLipo2Bindings.PWM4_PADS_B) });
                case 5:
                    return new Rp2350PwmBinding(PimoroniPicoLipo2Bindings.PWM5_BASE, PimoroniPicoLipo2Bindings.PWM5_RESET_MASK, PimoroniPicoLipo2Bindings.PWM5_SLICE, PimoroniPicoLipo2Bindings.PWM5_FUNCSEL,
                        PimoroniPicoLipo2Bindings.PWM5_CLK_SYS_HZ, new Rp2350PwmOutput[] {
                            new Rp2350PwmOutput(PimoroniPicoLipo2Bindings.PWM5_OUTPUT_A, PimoroniPicoLipo2Bindings.PWM5_IO_A_CTRL, PimoroniPicoLipo2Bindings.PWM5_PADS_A),
                            new Rp2350PwmOutput(PimoroniPicoLipo2Bindings.PWM5_OUTPUT_B, PimoroniPicoLipo2Bindings.PWM5_IO_B_CTRL, PimoroniPicoLipo2Bindings.PWM5_PADS_B) });
                case 6:
                    return new Rp2350PwmBinding(PimoroniPicoLipo2Bindings.PWM6_BASE, PimoroniPicoLipo2Bindings.PWM6_RESET_MASK, PimoroniPicoLipo2Bindings.PWM6_SLICE, PimoroniPicoLipo2Bindings.PWM6_FUNCSEL,
                        PimoroniPicoLipo2Bindings.PWM6_CLK_SYS_HZ, new Rp2350PwmOutput[] {
                            new Rp2350PwmOutput(PimoroniPicoLipo2Bindings.PWM6_OUTPUT_A, PimoroniPicoLipo2Bindings.PWM6_IO_A_CTRL, PimoroniPicoLipo2Bindings.PWM6_PADS_A),
                            new Rp2350PwmOutput(PimoroniPicoLipo2Bindings.PWM6_OUTPUT_B, PimoroniPicoLipo2Bindings.PWM6_IO_B_CTRL, PimoroniPicoLipo2Bindings.PWM6_PADS_B) });
                case 7:
                    return new Rp2350PwmBinding(PimoroniPicoLipo2Bindings.PWM7_BASE, PimoroniPicoLipo2Bindings.PWM7_RESET_MASK, PimoroniPicoLipo2Bindings.PWM7_SLICE, PimoroniPicoLipo2Bindings.PWM7_FUNCSEL,
                        PimoroniPicoLipo2Bindings.PWM7_CLK_SYS_HZ, new Rp2350PwmOutput[] {
                            new Rp2350PwmOutput(PimoroniPicoLipo2Bindings.PWM7_OUTPUT_A, PimoroniPicoLipo2Bindings.PWM7_IO_A_CTRL, PimoroniPicoLipo2Bindings.PWM7_PADS_A),
                            new Rp2350PwmOutput(PimoroniPicoLipo2Bindings.PWM7_OUTPUT_B, PimoroniPicoLipo2Bindings.PWM7_IO_B_CTRL, PimoroniPicoLipo2Bindings.PWM7_PADS_B) });
                default:
                    throw new ArgumentOutOfRangeException("slice");
            }
        }
#endif
    }
}
