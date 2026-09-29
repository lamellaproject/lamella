// Lamella.Boards.RaspberryPi.Pico2 -- the Raspberry Pi Pico 2 (RP2350A) board-support package.
using System;
using System.Device.Adc;
using System.Device.Analog;
using System.Device.Gpio;
using System.Device.I2c;
using System.Device.Pwm;
using System.Device.Spi;
using Lamella.Generated;
using Lamella.Hardware;

namespace Lamella.Boards.RaspberryPi
{
    public sealed class Pico2
    {
        /// <summary>The wire identity this board advertises (lamella_wire::product_model).</summary>
        public static readonly int BoardModel = RpiPico2Bindings.BOARD_MODEL;

        /// <summary>Constructs the board and ensures its clock tree is up (idempotent) -- so
        /// `new Pico2()` "just works" on both tiers: under an interpreter the resident firmware
        /// already raised clocks at boot (guard no-op), an AOT image raises them here.</summary>
        public Pico2()
        {
            EnsureClocks();
        }

        /// <summary>Binds this board's buses to the driver table, so a program writes plain
        /// dotnet/iot -- `SpiDevice.Create(settings)`, `new GpioController()` -- and never names
        /// a Lamella type. Touching `Pico2` at all is what arms it, which is why the samples
        /// construct the board first.</summary>
        /// <remarks>A TYPE INITIALIZER rather than the instance constructor, deliberately: the
        /// table refuses a second bind of the same id rather than replacing it, and this class is
        /// instantiable and routinely constructed as a temporary. The language runs a type
        /// initializer once per program, so idempotence costs nothing and the table keeps its
        /// throw as a genuine-error detector.
        /// The bound values are FACTORIES, not drivers, so a program that never touches a bus never
        /// constructs its driver. A driver's constructor touches no hardware either:
        /// `Rp2350AdcDriver` brings the converter up at the first channel opened or read.</remarks>
        static Pico2()
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
        private static GpioDriver MakeGpio() { return new Rp2350GpioDriver(); }
        private static AdcDriver MakeAdc() { return new Rp2350AdcDriver(AdcBinding()); }


        /// <summary>Brings the clock tree up if it is not already running -- idempotent. The no-op
        /// guard is CONJUNCTIVE (clk_sys on PLL_SYS AND both PLLs locked), so a PARTIAL state (e.g.
        /// after a soft restart: clk_sys on PLL_SYS but a PLL unlocked) takes the FULL bring-up,
        /// which parks clk_sys on clk_ref FIRST so re-cycling PLL_SYS never kills the clock the core
        /// is running on. Public so a boot stub or a test can call it explicitly.</summary>
        public static void EnsureClocks()
        {
            uint xoscCtrl = Rp2350Instances.XOSC_BASE + Rp2350XoscLayout.CTRL_OFF;
            uint xoscStatus = Rp2350Instances.XOSC_BASE + Rp2350XoscLayout.STATUS_OFF;
            uint xoscStartup = Rp2350Instances.XOSC_BASE + Rp2350XoscLayout.STARTUP_OFF;
            uint clkRefCtrl = Rp2350Instances.CLOCKS_BASE + Rp2350ClocksLayout.CLK_REF_CTRL_OFF;
            uint clkRefSelected = Rp2350Instances.CLOCKS_BASE + Rp2350ClocksLayout.CLK_REF_SELECTED_OFF;
            uint clkSysCtrl = Rp2350Instances.CLOCKS_BASE + Rp2350ClocksLayout.CLK_SYS_CTRL_OFF;
            uint clkSysSelected = Rp2350Instances.CLOCKS_BASE + Rp2350ClocksLayout.CLK_SYS_SELECTED_OFF;
            uint clkUsbCtrl = Rp2350Instances.CLOCKS_BASE + Rp2350ClocksLayout.CLK_USB_CTRL_OFF;
            uint pllSysCs = Rp2350Instances.PLL_SYS_BASE + Rp2350PllLayout.CS_OFF;
            uint pllUsbCs = Rp2350Instances.PLL_USB_BASE + Rp2350PllLayout.CS_OFF;

            bool sysOnPll = (Mmio.Read32(clkSysSelected) & Rp2350ClocksLayout.CLK_SYS_AUX_SELECTED) != 0u;
            bool sysLocked = (Mmio.Read32(pllSysCs) & Rp2350PllLayout.CS_LOCK) != 0u;
            bool usbLocked = (Mmio.Read32(pllUsbCs) & Rp2350PllLayout.CS_LOCK) != 0u;
            if (sysOnPll && sysLocked && usbLocked) return;

            Mmio.Write32(xoscStartup, Rp2350XoscLayout.STARTUP_DELAY_1MS);
            Mmio.Write32(xoscCtrl,
                (Rp2350XoscLayout.CTRL_ENABLE_MAGIC << (int)Rp2350XoscLayout.CTRL_ENABLE_LSB)
                | Rp2350XoscLayout.CTRL_FREQ_RANGE_1_15MHZ);
            for (int spin = 0; spin < 1000000; spin++)
            {
                if ((Mmio.Read32(xoscStatus) & Rp2350XoscLayout.STATUS_STABLE) != 0u) break;
            }

            Mmio.Write32(clkRefCtrl,
                (Mmio.Read32(clkRefCtrl) & ~Rp2350ClocksLayout.CLK_REF_CTRL_SRC) | Rp2350ClocksLayout.CLK_REF_SRC_XOSC);
            for (int spin = 0; spin < 100000; spin++)
            {
                if ((Mmio.Read32(clkRefSelected) & Rp2350ClocksLayout.CLK_REF_XOSC_SELECTED) != 0u) break;
            }
            Mmio.Write32(clkSysCtrl, Mmio.Read32(clkSysCtrl) & ~Rp2350ClocksLayout.CLK_SYS_CTRL_SRC);
            bool parked = false;
            for (int spin = 0; spin < 100000; spin++)
            {
                if ((Mmio.Read32(clkSysSelected) & Rp2350ClocksLayout.CLK_SYS_REF_SELECTED) != 0u)
                {
                    parked = true;
                    break;
                }
            }
            if (!parked) return;

            InitPll(pllSysCs,
                Rp2350Instances.PLL_SYS_BASE + Rp2350PllLayout.FBDIV_INT_OFF,
                Rp2350Instances.PLL_SYS_BASE + Rp2350PllLayout.PRIM_OFF,
                Rp2350Instances.PLL_SYS_BASE + Rp2350PllLayout.PWR_CLR_OFF,
                Rp2350Instances.PLL_SYS_RESET_MASK,
                RpiPico2Bindings.PLL_SYS_FBDIV_PLL_150_48, RpiPico2Bindings.PLL_SYS_PRIM_PLL_150_48);
            Mmio.Write32(clkSysCtrl, 0);
            Mmio.Write32(clkSysCtrl, Rp2350ClocksLayout.CLK_SYS_SRC_AUX);
            for (int spin = 0; spin < 100000; spin++)
            {
                if ((Mmio.Read32(clkSysSelected) & Rp2350ClocksLayout.CLK_SYS_AUX_SELECTED) != 0u) break;
            }

            InitPll(pllUsbCs,
                Rp2350Instances.PLL_USB_BASE + Rp2350PllLayout.FBDIV_INT_OFF,
                Rp2350Instances.PLL_USB_BASE + Rp2350PllLayout.PRIM_OFF,
                Rp2350Instances.PLL_USB_BASE + Rp2350PllLayout.PWR_CLR_OFF,
                Rp2350Instances.PLL_USB_RESET_MASK,
                RpiPico2Bindings.PLL_USB_FBDIV_PLL_150_48, RpiPico2Bindings.PLL_USB_PRIM_PLL_150_48);
            Mmio.Write32(clkUsbCtrl, 0);
            Mmio.Write32(clkUsbCtrl, Rp2350ClocksLayout.CLK_USB_CTRL_ENABLE);
        }

        static void InitPll(uint cs, uint fbdiv, uint prim, uint pwrClr, uint resetMask, uint fbdivValue, uint primValue)
        {
            ResetCycle(resetMask);
            Mmio.Write32(cs, 1u << (int)Rp2350PllLayout.CS_REFDIV_LSB);
            Mmio.Write32(fbdiv, fbdivValue);
            Mmio.Write32(pwrClr, Rp2350PllLayout.PWR_PD | Rp2350PllLayout.PWR_VCOPD);
            for (int spin = 0; spin < 1000000; spin++)
            {
                if ((Mmio.Read32(cs) & Rp2350PllLayout.CS_LOCK) != 0u) break;
            }
            Mmio.Write32(prim, primValue);
            Mmio.Write32(pwrClr, Rp2350PllLayout.PWR_POSTDIVPD);
        }

        static void ResetCycle(uint resetMask)
        {
            uint resetsSet = Rp2350Instances.RESETS_BASE + Rp2350ResetsLayout.RESET_SET_OFF;
            uint resetsClr = Rp2350Instances.RESETS_CLR_BASE + Rp2350ResetsLayout.RESET_OFF;
            uint resetsDone = Rp2350Instances.RESETS_BASE + Rp2350ResetsLayout.RESET_DONE_OFF;
            Mmio.Write32(resetsSet, resetMask);
            for (int spin = 0; spin < 100000; spin++)
            {
                if ((Mmio.Read32(resetsDone) & resetMask) == 0u) break;
            }
            Mmio.Write32(resetsClr, resetMask);
            for (int spin = 0; spin < 100000; spin++)
            {
                if ((Mmio.Read32(resetsDone) & resetMask) != 0u) break;
            }
        }

        public static readonly int TemperatureSensorChannel = (int)RpiPico2Bindings.ADC_TEMPERATURE_CHANNEL;
        public static readonly int AdcChannelGp26 = (int)RpiPico2Bindings.ADC_GPIO26_CHANNEL;
        public static readonly int AdcChannelGp27 = (int)RpiPico2Bindings.ADC_GPIO27_CHANNEL;
        public static readonly int AdcChannelGp28 = (int)RpiPico2Bindings.ADC_GPIO28_CHANNEL;
        public static readonly int AdcChannelGp29 = (int)RpiPico2Bindings.ADC_GPIO29_CHANNEL;
        public static readonly int AdcReferenceMicrovolts = (int)RpiPico2Bindings.ADC_REFERENCE_UV;

        /// <summary>The `adc` binding descriptor, built from the generated constants.</summary>
        public Rp2350AdcBinding CreateAdcBinding() { return AdcBinding(); }

        private static Rp2350AdcBinding AdcBinding()
        {
            return new Rp2350AdcBinding(
                RpiPico2Bindings.ADC_BASE,
                RpiPico2Bindings.ADC_RESET_MASK,
                RpiPico2Bindings.ADC_REFERENCE_UV,
                (int)RpiPico2Bindings.ADC_CHANNEL_COUNT,
                (int)RpiPico2Bindings.ADC_TEMPERATURE_CHANNEL,
                new int[] {
                    (int)RpiPico2Bindings.ADC_CHANNEL0_PIN, (int)RpiPico2Bindings.ADC_CHANNEL1_PIN,
                    (int)RpiPico2Bindings.ADC_CHANNEL2_PIN, (int)RpiPico2Bindings.ADC_CHANNEL3_PIN },
                RpiPico2Bindings.ADC_RESERVED_CHANNELS,
                new string[4],
                new uint[4],
                new uint[4]);
        }

        /// <summary>An ADC controller over the RP2350 SAR converter (the on-chip temperature
        /// sensor is on <see cref="TemperatureSensorChannel"/>).</summary>
        public AdcController CreateAdcController()
        {
            return new AdcController();
        }

        /// <summary>The on-chip converter as dotnet/iot's analog controller. Each pin is a converter
        /// channel: <see cref="AdcChannelGp26"/> to <see cref="AdcChannelGp29"/> and
        /// <see cref="TemperatureSensorChannel"/> name them.</summary>
        /// <remarks>Creating it touches no hardware; the first pin opened brings the converter up.
        /// It reads through the same driver as <see cref="CreateAdcController"/>.</remarks>
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
                RpiPico2Bindings.UART0_BASE,
                RpiPico2Bindings.UART0_RESET_MASK,
                RpiPico2Bindings.UART0_IO_TX_CTRL,
                RpiPico2Bindings.UART0_IO_RX_CTRL,
                RpiPico2Bindings.UART0_PADS_TX,
                RpiPico2Bindings.UART0_PADS_RX,
                RpiPico2Bindings.UART0_FUNCSEL,
                RpiPico2Bindings.UART0_CLK_PERI_HZ);
        }

        /// <summary>UART0 on GP0 (TX, header pin 1) / GP1 (RX, pin 2), ready for
        /// <c>Init(baud)</c>.</summary>
        public Rp2350Uart CreateUart()
        {
            return new Rp2350Uart(CreateUartBinding());
        }

        /// <summary>The `spi0` binding descriptor for <paramref name="busId"/>
        /// (bus 0 = SPI0 on GP16 MISO / GP17 CS / GP18 SCK / GP19 MOSI; unknown ids
        /// refuse loudly).</summary>
        public Rp2350SpiBinding CreateSpiBinding(int busId) { return SpiBinding(busId); }

        private static Rp2350SpiBinding SpiBinding(int busId)
        {
            if (busId != 0)
            {
                throw new ArgumentException("pico2 has no such spi bus: bus 0 = SPI0 on GP16..GP19");
            }
            return new Rp2350SpiBinding(
                RpiPico2Bindings.SPI0_BASE,
                RpiPico2Bindings.SPI0_RESET_MASK,
                RpiPico2Bindings.SPI0_IO_MISO_CTRL,
                RpiPico2Bindings.SPI0_PADS_MISO,
                RpiPico2Bindings.SPI0_IO_CS_CTRL,
                RpiPico2Bindings.SPI0_PADS_CS,
                RpiPico2Bindings.SPI0_IO_SCK_CTRL,
                RpiPico2Bindings.SPI0_PADS_SCK,
                RpiPico2Bindings.SPI0_IO_MOSI_CTRL,
                RpiPico2Bindings.SPI0_PADS_MOSI,
                RpiPico2Bindings.SPI0_FUNCSEL,
                RpiPico2Bindings.SPI0_SSPCLK_HZ);
        }

        /// <summary>A SPI device per <paramref name="settings"/>: the settings' BusId picks
        /// the descriptor. A negative ChipSelectLine routes the bus's hardware CS pin
        /// as the PL022's ss_n; a non-negative line is driven as a managed SIO chip-select.</summary>
        public SpiDevice CreateSpiDevice(SpiConnectionSettings settings)
        {
            return SpiDevice.Create(settings);
        }

        /// <summary>A SPI device on bus 0 with <paramref name="chipSelectLine"/> (the
        /// one-argument convenience the demos use).</summary>
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
                throw new ArgumentException("pico2 has no such i2c bus: bus 0 = I2C0 on GP4/GP5");
            }
            return new Rp2350I2cBinding(
                RpiPico2Bindings.I2C0_BASE,
                RpiPico2Bindings.I2C0_RESET_MASK,
                RpiPico2Bindings.I2C0_IO_SDA_CTRL,
                RpiPico2Bindings.I2C0_PADS_SDA,
                RpiPico2Bindings.I2C0_IO_SCL_CTRL,
                RpiPico2Bindings.I2C0_PADS_SCL,
                RpiPico2Bindings.I2C0_FUNCSEL,
                RpiPico2Bindings.I2C0_IC_CLK_HZ);
        }

        /// <summary>An I2C device per <paramref name="settings"/>: the settings' BusId picks
        /// the descriptor (the id stops being decorative).</summary>
        public I2cDevice CreateI2cDevice(I2cConnectionSettings settings)
        {
            return I2cDevice.Create(settings);
        }

        /// <summary>An I2C device on bus 0 (I2C0, GP4/GP5) at <paramref name="deviceAddress"/>.</summary>
        public I2cDevice CreateI2cDevice(int deviceAddress)
        {
            return CreateI2cDevice(new I2cConnectionSettings(0, deviceAddress));
        }

        /// <summary>A GPIO controller over the RP2350 SIO/pad block.</summary>
        public GpioController CreateGpioController()
        {
            return new GpioController();
        }

        /// <summary>Converts a temperature-sensor microvolt reading to milli-degrees Celsius using
        /// the GENERATED vbe-linear calibration record (the adc block's strata): the integer form
        /// both language skins share, T_mC = t0 + (v_uV - vbe) * 1000 / slope (truncating; slope is
        /// negative, so rising voltage falls in temperature). Demos call this instead of inlining
        /// the coefficients.</summary>
        public static int TemperatureMilliCelsius(long microvolts)
        {
            long t0 = Rp2350AdcLayout.TemperatureSensor_T0Millicelsius;
            long vbe = Rp2350AdcLayout.TemperatureSensor_VbeAtT0Microvolts;
            long slope = Rp2350AdcLayout.TemperatureSensor_SlopeMicrovoltsPerCelsius;
            return (int)(t0 + (microvolts - vbe) * 1000L / slope);
        }

        /// <summary>Microvolts from a raw ADC count, using the generated board reference voltage
        /// over <paramref name="fullScaleCounts"/> (truncating). For an averaged sum, pass the
        /// sum as <paramref name="raw"/> and counts * samples as the scale.</summary>
        public static long MicrovoltsFromRaw(long raw, long fullScaleCounts)
        {
            return raw * RpiPico2Bindings.ADC_REFERENCE_UV / fullScaleCounts;
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
                    return new Rp2350PwmBinding(RpiPico2Bindings.PWM1_BASE, RpiPico2Bindings.PWM1_RESET_MASK, RpiPico2Bindings.PWM1_SLICE, RpiPico2Bindings.PWM1_FUNCSEL,
                        RpiPico2Bindings.PWM1_CLK_SYS_HZ, new Rp2350PwmOutput[] {
                            new Rp2350PwmOutput(RpiPico2Bindings.PWM1_OUTPUT_A, RpiPico2Bindings.PWM1_IO_A_CTRL, RpiPico2Bindings.PWM1_PADS_A),
                            new Rp2350PwmOutput(RpiPico2Bindings.PWM1_OUTPUT_B, RpiPico2Bindings.PWM1_IO_B_CTRL, RpiPico2Bindings.PWM1_PADS_B) });
                case 2:
                    return new Rp2350PwmBinding(RpiPico2Bindings.PWM2_BASE, RpiPico2Bindings.PWM2_RESET_MASK, RpiPico2Bindings.PWM2_SLICE, RpiPico2Bindings.PWM2_FUNCSEL,
                        RpiPico2Bindings.PWM2_CLK_SYS_HZ, new Rp2350PwmOutput[] {
                            new Rp2350PwmOutput(RpiPico2Bindings.PWM2_OUTPUT_A, RpiPico2Bindings.PWM2_IO_A_CTRL, RpiPico2Bindings.PWM2_PADS_A),
                            new Rp2350PwmOutput(RpiPico2Bindings.PWM2_OUTPUT_B, RpiPico2Bindings.PWM2_IO_B_CTRL, RpiPico2Bindings.PWM2_PADS_B) });
                case 3:
                    return new Rp2350PwmBinding(RpiPico2Bindings.PWM3_BASE, RpiPico2Bindings.PWM3_RESET_MASK, RpiPico2Bindings.PWM3_SLICE, RpiPico2Bindings.PWM3_FUNCSEL,
                        RpiPico2Bindings.PWM3_CLK_SYS_HZ, new Rp2350PwmOutput[] {
                            new Rp2350PwmOutput(RpiPico2Bindings.PWM3_OUTPUT_A, RpiPico2Bindings.PWM3_IO_A_CTRL, RpiPico2Bindings.PWM3_PADS_A),
                            new Rp2350PwmOutput(RpiPico2Bindings.PWM3_OUTPUT_B, RpiPico2Bindings.PWM3_IO_B_CTRL, RpiPico2Bindings.PWM3_PADS_B) });
                case 4:
                    return new Rp2350PwmBinding(RpiPico2Bindings.PWM4_BASE, RpiPico2Bindings.PWM4_RESET_MASK, RpiPico2Bindings.PWM4_SLICE, RpiPico2Bindings.PWM4_FUNCSEL,
                        RpiPico2Bindings.PWM4_CLK_SYS_HZ, new Rp2350PwmOutput[] {
                            new Rp2350PwmOutput(RpiPico2Bindings.PWM4_OUTPUT_A, RpiPico2Bindings.PWM4_IO_A_CTRL, RpiPico2Bindings.PWM4_PADS_A),
                            new Rp2350PwmOutput(RpiPico2Bindings.PWM4_OUTPUT_B, RpiPico2Bindings.PWM4_IO_B_CTRL, RpiPico2Bindings.PWM4_PADS_B) });
                case 5:
                    return new Rp2350PwmBinding(RpiPico2Bindings.PWM5_BASE, RpiPico2Bindings.PWM5_RESET_MASK, RpiPico2Bindings.PWM5_SLICE, RpiPico2Bindings.PWM5_FUNCSEL,
                        RpiPico2Bindings.PWM5_CLK_SYS_HZ, new Rp2350PwmOutput[] {
                            new Rp2350PwmOutput(RpiPico2Bindings.PWM5_OUTPUT_A, RpiPico2Bindings.PWM5_IO_A_CTRL, RpiPico2Bindings.PWM5_PADS_A),
                            new Rp2350PwmOutput(RpiPico2Bindings.PWM5_OUTPUT_B, RpiPico2Bindings.PWM5_IO_B_CTRL, RpiPico2Bindings.PWM5_PADS_B) });
                case 6:
                    return new Rp2350PwmBinding(RpiPico2Bindings.PWM6_BASE, RpiPico2Bindings.PWM6_RESET_MASK, RpiPico2Bindings.PWM6_SLICE, RpiPico2Bindings.PWM6_FUNCSEL,
                        RpiPico2Bindings.PWM6_CLK_SYS_HZ, new Rp2350PwmOutput[] {
                            new Rp2350PwmOutput(RpiPico2Bindings.PWM6_OUTPUT_A, RpiPico2Bindings.PWM6_IO_A_CTRL, RpiPico2Bindings.PWM6_PADS_A),
                            new Rp2350PwmOutput(RpiPico2Bindings.PWM6_OUTPUT_B, RpiPico2Bindings.PWM6_IO_B_CTRL, RpiPico2Bindings.PWM6_PADS_B) });
                case 7:
                    return new Rp2350PwmBinding(RpiPico2Bindings.PWM7_BASE, RpiPico2Bindings.PWM7_RESET_MASK, RpiPico2Bindings.PWM7_SLICE, RpiPico2Bindings.PWM7_FUNCSEL,
                        RpiPico2Bindings.PWM7_CLK_SYS_HZ, new Rp2350PwmOutput[] {
                            new Rp2350PwmOutput(RpiPico2Bindings.PWM7_OUTPUT_A, RpiPico2Bindings.PWM7_IO_A_CTRL, RpiPico2Bindings.PWM7_PADS_A),
                            new Rp2350PwmOutput(RpiPico2Bindings.PWM7_OUTPUT_B, RpiPico2Bindings.PWM7_IO_B_CTRL, RpiPico2Bindings.PWM7_PADS_B) });
                default:
                    throw new ArgumentOutOfRangeException("slice");
            }
        }
#endif
    }
}
