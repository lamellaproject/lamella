// Lamella.Boards.RaspberryPi.Pico2W -- the Raspberry Pi Pico 2 W (RP2350A + an Infineon CYW43439).
using System;
using System.Device.Analog;
using System.Device.Gpio;
using System.Device.I2c;
using System.Device.Pwm;
using System.Device.Spi;
using Lamella.Generated;
using Lamella.Hardware;

namespace Lamella.Boards.RaspberryPi
{
    internal sealed class Pico2W
    {
        /// <summary>The wire identity this board advertises (lamella_wire::board_model).</summary>
        public static readonly int BoardModel = RaspberrypiPico2WBindings.BOARD_MODEL;

        /// <summary>The pins the CYW43439 owns, as a mask over bank 0 -- WL_REG_ON, the shared
        /// data/IRQ line, the chip select and the clock. Composed from the generated per-line masks
        /// rather than written out, so a line moved in board.toml moves here.</summary>
        public static readonly uint RadioPins =
            RaspberrypiPico2WBindings.CYW43439_WL_REG_ON_MASK
            | RaspberrypiPico2WBindings.CYW43439_DATA_MASK
            | RaspberrypiPico2WBindings.CYW43439_CS_MASK
            | RaspberrypiPico2WBindings.CYW43439_CLK_MASK;

        /// <summary>Binds this board's buses to the driver table, so a program writes plain
        /// dotnet/iot -- <c>SpiDevice.Create(settings)</c>, <c>new GpioController()</c> -- and never
        /// names a Lamella type. Touching <see cref="Pico2W"/> at all is what arms it, and constructing
        /// <see cref="TargetBoard"/> touches it.</summary>
        /// <remarks>A TYPE INITIALIZER rather than the instance constructor, for the reason
        /// <see cref="Lamella.Hardware.Buses.BindGpio"/> documents: the table refuses a second bind
        /// of the same kind rather than replacing it, and this class is instantiable and routinely
        /// constructed as a temporary. The language runs a type initializer once per program, so
        /// idempotence costs nothing and the table keeps its throw as a genuine-error detector.
        /// The bound values are FACTORIES, not drivers, so a program that never touches a bus never
        /// constructs its driver.</remarks>
        static Pico2W()
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
        private static GpioDriver MakeGpio() { return new Rp2350GpioDriver(RadioPins); }
        private static AdcDriver MakeAdc() { return new Rp2350AdcDriver(AdcBinding()); }

        /// <summary>The family SIO/pad driver this board bound, over bank 0 less the four lines in
        /// <see cref="RadioPins"/>.</summary>
        /// <remarks>THE SAME INSTANCE <see cref="GpioController"/> drives. One block has one
        /// driver, and handing out a second one over the same registers reads as working while the
        /// facade talks to the first -- see <see cref="Lamella.Hardware.Buses.ResolveSpi"/> for the
        /// full argument.</remarks>
        public GpioDriver CreateGpioDriver()
        {
            return Buses.ResolveGpio();
        }

        /// <summary>A GPIO controller over the RP2350 SIO/pad block. The radio's four lines refuse
        /// <c>SetPinMode</c>.</summary>
        public GpioController CreateGpioController()
        {
            return new GpioController();
        }

        /// <summary>The on-chip temperature sensor's converter channel.</summary>
        public static readonly int TemperatureSensorChannel = (int)RaspberrypiPico2WBindings.ADC_TEMPERATURE_CHANNEL;
        /// <summary>The converter channel that reads GP26, header pin 31.</summary>
        public static readonly int AdcChannelGp26 = (int)RaspberrypiPico2WBindings.ADC_GPIO26_CHANNEL;
        /// <summary>The converter channel that reads GP27, header pin 32.</summary>
        public static readonly int AdcChannelGp27 = (int)RaspberrypiPico2WBindings.ADC_GPIO27_CHANNEL;
        /// <summary>The converter channel that reads GP28, header pin 34.</summary>
        public static readonly int AdcChannelGp28 = (int)RaspberrypiPico2WBindings.ADC_GPIO28_CHANNEL;
        /// <summary>The board's converter reference, in microvolts.</summary>
        public static readonly int AdcReferenceMicrovolts = (int)RaspberrypiPico2WBindings.ADC_REFERENCE_UV;

        /// <summary>The `adc` binding descriptor, built from the generated constants.</summary>
        public Rp2350AdcBinding CreateAdcBinding() { return AdcBinding(); }

        private static Rp2350AdcBinding AdcBinding()
        {
            return new Rp2350AdcBinding(
                RaspberrypiPico2WBindings.ADC_BASE,
                RaspberrypiPico2WBindings.ADC_RESET_MASK,
                RaspberrypiPico2WBindings.ADC_REFERENCE_UV,
                (int)RaspberrypiPico2WBindings.ADC_CHANNEL_COUNT,
                (int)RaspberrypiPico2WBindings.ADC_TEMPERATURE_CHANNEL,
                new int[] {
                    (int)RaspberrypiPico2WBindings.ADC_CHANNEL0_PIN, (int)RaspberrypiPico2WBindings.ADC_CHANNEL1_PIN,
                    (int)RaspberrypiPico2WBindings.ADC_CHANNEL2_PIN, (int)RaspberrypiPico2WBindings.ADC_CHANNEL3_PIN },
                RaspberrypiPico2WBindings.ADC_RESERVED_CHANNELS,
                new string[] { null, null, null, RaspberrypiPico2WBindings.ADC_CHANNEL3_RESERVED_BY },
                new uint[4],
                new uint[4]);
        }

        /// <summary>The on-chip converter as dotnet/iot's analog controller. A pin is opened by its GPIO
        /// number, GP26 to GP28: <c>OpenPin(26)</c> reads GP26, converter channel
        /// <see cref="AdcChannelGp26"/>. The temperature sensor's channel reads no pin, so it does not
        /// open here.</summary>
        /// <remarks>Creating it touches no hardware; the first pin opened brings the converter up.
        /// It reads through the converter driver the board binds.
        /// GP29 is the radio's clock line and refuses to open.</remarks>
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
                RaspberrypiPico2WBindings.UART0_BASE,
                RaspberrypiPico2WBindings.UART0_RESET_MASK,
                RaspberrypiPico2WBindings.UART0_IO_TX_CTRL,
                RaspberrypiPico2WBindings.UART0_IO_RX_CTRL,
                RaspberrypiPico2WBindings.UART0_PADS_TX,
                RaspberrypiPico2WBindings.UART0_PADS_RX,
                RaspberrypiPico2WBindings.UART0_FUNCSEL,
                RaspberrypiPico2WBindings.UART0_CLK_PERI_HZ);
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
                throw new ArgumentException("pico2-w has no such spi bus: bus 0 = SPI0 on GP16..GP19");
            }
            return new Rp2350SpiBinding(
                RaspberrypiPico2WBindings.SPI0_BASE,
                RaspberrypiPico2WBindings.SPI0_RESET_MASK,
                RaspberrypiPico2WBindings.SPI0_IO_MISO_CTRL,
                RaspberrypiPico2WBindings.SPI0_PADS_MISO,
                RaspberrypiPico2WBindings.SPI0_IO_SCK_CTRL,
                RaspberrypiPico2WBindings.SPI0_PADS_SCK,
                RaspberrypiPico2WBindings.SPI0_IO_MOSI_CTRL,
                RaspberrypiPico2WBindings.SPI0_PADS_MOSI,
                RaspberrypiPico2WBindings.SPI0_FUNCSEL,
                RaspberrypiPico2WBindings.SPI0_SSPCLK_HZ,
                new int[] { (int)RaspberrypiPico2WBindings.SPI0_CHIP_SELECT0 });
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
        /// (bus 0 = I2C0 on GP4 SDA / GP5 SCL; unknown ids refuse loudly).</summary>
        public Rp2350I2cBinding CreateI2cBinding(int busId) { return I2cBinding(busId); }

        private static Rp2350I2cBinding I2cBinding(int busId)
        {
            if (busId != 0)
            {
                throw new ArgumentException("pico2-w has no such i2c bus: bus 0 = I2C0 on GP4/GP5");
            }
            return new Rp2350I2cBinding(
                RaspberrypiPico2WBindings.I2C0_BASE,
                RaspberrypiPico2WBindings.I2C0_RESET_MASK,
                RaspberrypiPico2WBindings.I2C0_IO_SDA_CTRL,
                RaspberrypiPico2WBindings.I2C0_PADS_SDA,
                RaspberrypiPico2WBindings.I2C0_IO_SCL_CTRL,
                RaspberrypiPico2WBindings.I2C0_PADS_SCL,
                RaspberrypiPico2WBindings.I2C0_FUNCSEL,
                RaspberrypiPico2WBindings.I2C0_IC_CLK_HZ);
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
                _pwmSlices[slice] = Rp2350PwmSlice.Of(PwmBinding(slice));
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
                    return new Rp2350PwmBinding(RaspberrypiPico2WBindings.PWM1_BASE, RaspberrypiPico2WBindings.PWM1_RESET_MASK, RaspberrypiPico2WBindings.PWM1_SLICE, RaspberrypiPico2WBindings.PWM1_FUNCSEL,
                        RaspberrypiPico2WBindings.PWM1_CLK_SYS_HZ, new Rp2350PwmOutput[] {
                            new Rp2350PwmOutput(RaspberrypiPico2WBindings.PWM1_OUTPUT_A, RaspberrypiPico2WBindings.PWM1_IO_A_CTRL, RaspberrypiPico2WBindings.PWM1_PADS_A),
                            new Rp2350PwmOutput(RaspberrypiPico2WBindings.PWM1_OUTPUT_B, RaspberrypiPico2WBindings.PWM1_IO_B_CTRL, RaspberrypiPico2WBindings.PWM1_PADS_B) });
                case 2:
                    return new Rp2350PwmBinding(RaspberrypiPico2WBindings.PWM2_BASE, RaspberrypiPico2WBindings.PWM2_RESET_MASK, RaspberrypiPico2WBindings.PWM2_SLICE, RaspberrypiPico2WBindings.PWM2_FUNCSEL,
                        RaspberrypiPico2WBindings.PWM2_CLK_SYS_HZ, new Rp2350PwmOutput[] {
                            new Rp2350PwmOutput(RaspberrypiPico2WBindings.PWM2_OUTPUT_A, RaspberrypiPico2WBindings.PWM2_IO_A_CTRL, RaspberrypiPico2WBindings.PWM2_PADS_A),
                            new Rp2350PwmOutput(RaspberrypiPico2WBindings.PWM2_OUTPUT_B, RaspberrypiPico2WBindings.PWM2_IO_B_CTRL, RaspberrypiPico2WBindings.PWM2_PADS_B) });
                case 3:
                    return new Rp2350PwmBinding(RaspberrypiPico2WBindings.PWM3_BASE, RaspberrypiPico2WBindings.PWM3_RESET_MASK, RaspberrypiPico2WBindings.PWM3_SLICE, RaspberrypiPico2WBindings.PWM3_FUNCSEL,
                        RaspberrypiPico2WBindings.PWM3_CLK_SYS_HZ, new Rp2350PwmOutput[] {
                            new Rp2350PwmOutput(RaspberrypiPico2WBindings.PWM3_OUTPUT_A, RaspberrypiPico2WBindings.PWM3_IO_A_CTRL, RaspberrypiPico2WBindings.PWM3_PADS_A),
                            new Rp2350PwmOutput(RaspberrypiPico2WBindings.PWM3_OUTPUT_B, RaspberrypiPico2WBindings.PWM3_IO_B_CTRL, RaspberrypiPico2WBindings.PWM3_PADS_B) });
                case 4:
                    return new Rp2350PwmBinding(RaspberrypiPico2WBindings.PWM4_BASE, RaspberrypiPico2WBindings.PWM4_RESET_MASK, RaspberrypiPico2WBindings.PWM4_SLICE, RaspberrypiPico2WBindings.PWM4_FUNCSEL,
                        RaspberrypiPico2WBindings.PWM4_CLK_SYS_HZ, new Rp2350PwmOutput[] {
                            new Rp2350PwmOutput(RaspberrypiPico2WBindings.PWM4_OUTPUT_A, RaspberrypiPico2WBindings.PWM4_IO_A_CTRL, RaspberrypiPico2WBindings.PWM4_PADS_A),
                            new Rp2350PwmOutput(RaspberrypiPico2WBindings.PWM4_OUTPUT_B, RaspberrypiPico2WBindings.PWM4_IO_B_CTRL, RaspberrypiPico2WBindings.PWM4_PADS_B) });
                case 5:
                    return new Rp2350PwmBinding(RaspberrypiPico2WBindings.PWM5_BASE, RaspberrypiPico2WBindings.PWM5_RESET_MASK, RaspberrypiPico2WBindings.PWM5_SLICE, RaspberrypiPico2WBindings.PWM5_FUNCSEL,
                        RaspberrypiPico2WBindings.PWM5_CLK_SYS_HZ, new Rp2350PwmOutput[] {
                            new Rp2350PwmOutput(RaspberrypiPico2WBindings.PWM5_OUTPUT_A, RaspberrypiPico2WBindings.PWM5_IO_A_CTRL, RaspberrypiPico2WBindings.PWM5_PADS_A),
                            new Rp2350PwmOutput(RaspberrypiPico2WBindings.PWM5_OUTPUT_B, RaspberrypiPico2WBindings.PWM5_IO_B_CTRL, RaspberrypiPico2WBindings.PWM5_PADS_B) });
                case 6:
                    return new Rp2350PwmBinding(RaspberrypiPico2WBindings.PWM6_BASE, RaspberrypiPico2WBindings.PWM6_RESET_MASK, RaspberrypiPico2WBindings.PWM6_SLICE, RaspberrypiPico2WBindings.PWM6_FUNCSEL,
                        RaspberrypiPico2WBindings.PWM6_CLK_SYS_HZ, new Rp2350PwmOutput[] {
                            new Rp2350PwmOutput(RaspberrypiPico2WBindings.PWM6_OUTPUT_A, RaspberrypiPico2WBindings.PWM6_IO_A_CTRL, RaspberrypiPico2WBindings.PWM6_PADS_A),
                            new Rp2350PwmOutput(RaspberrypiPico2WBindings.PWM6_OUTPUT_B, RaspberrypiPico2WBindings.PWM6_IO_B_CTRL, RaspberrypiPico2WBindings.PWM6_PADS_B) });
                case 7:
                    return new Rp2350PwmBinding(RaspberrypiPico2WBindings.PWM7_BASE, RaspberrypiPico2WBindings.PWM7_RESET_MASK, RaspberrypiPico2WBindings.PWM7_SLICE, RaspberrypiPico2WBindings.PWM7_FUNCSEL,
                        RaspberrypiPico2WBindings.PWM7_CLK_SYS_HZ, new Rp2350PwmOutput[] {
                            new Rp2350PwmOutput(RaspberrypiPico2WBindings.PWM7_OUTPUT_A, RaspberrypiPico2WBindings.PWM7_IO_A_CTRL, RaspberrypiPico2WBindings.PWM7_PADS_A),
                            new Rp2350PwmOutput(RaspberrypiPico2WBindings.PWM7_OUTPUT_B, RaspberrypiPico2WBindings.PWM7_IO_B_CTRL, RaspberrypiPico2WBindings.PWM7_PADS_B) });
                default:
                    throw new ArgumentOutOfRangeException("slice");
            }
        }
#endif
    }
}
