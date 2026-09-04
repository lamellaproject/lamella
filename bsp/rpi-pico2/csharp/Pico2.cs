// Lamella.Boards.RaspberryPi.Pico2 -- the Raspberry Pi Pico 2 (RP2350A) board-support package.
using System;
using System.Device.Adc;
using System.Device.Gpio;
using System.Device.I2c;
using System.Device.Spi;
using Lamella.Generated;
using Lamella.Hardware;

namespace Lamella.Boards.RaspberryPi
{
    public sealed class Pico2
    {
        /// <summary>The wire identity this board advertises (lamella_wire::product_model).</summary>
        public static readonly int BoardModel = Pico2Bindings.BOARD_MODEL;

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
        /// The bound values are FACTORIES, not drivers: `Rp2350AdcDriver`'s constructor brings the
        /// converter up and assumes PLL_USB is already running, so constructing every driver here
        /// would spin up a temperature sensor for a program that only blinks an LED.</remarks>
        static Pico2()
        {
            Buses.BindSpi(0, new SpiDriverFactory(MakeSpi0));
            Buses.BindI2c(0, new I2cDriverFactory(MakeI2c0));
            Buses.BindGpio(new GpioDriverFactory(MakeGpio));
            AdcControllers.Bind(new AdcDriverFactory(MakeAdc));
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

            Mmio.Write32(clkRefCtrl, Rp2350ClocksLayout.CLK_REF_SRC_XOSC);
            for (int spin = 0; spin < 100000; spin++)
            {
                if ((Mmio.Read32(clkRefSelected) & Rp2350ClocksLayout.CLK_REF_XOSC_SELECTED) != 0u) break;
            }
            Mmio.Write32(clkSysCtrl, 0);
            for (int spin = 0; spin < 100000; spin++)
            {
                if ((Mmio.Read32(clkSysSelected) & Rp2350ClocksLayout.CLK_SYS_REF_SELECTED) != 0u) break;
            }

            InitPll(pllSysCs,
                Rp2350Instances.PLL_SYS_BASE + Rp2350PllLayout.FBDIV_INT_OFF,
                Rp2350Instances.PLL_SYS_BASE + Rp2350PllLayout.PRIM_OFF,
                Rp2350Instances.PLL_SYS_BASE + Rp2350PllLayout.PWR_CLR_OFF,
                Rp2350Instances.PLL_SYS_RESET_MASK,
                Pico2Bindings.PLL_SYS_FBDIV_PLL_150_48, Pico2Bindings.PLL_SYS_PRIM_PLL_150_48);
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
                Pico2Bindings.PLL_USB_FBDIV_PLL_150_48, Pico2Bindings.PLL_USB_PRIM_PLL_150_48);
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

        public static readonly int TemperatureSensorChannel = Rp2350AdcLayout.Channel_TemperatureSensor;
        public static readonly int AdcChannelGp26 = Rp2350AdcLayout.Channel_GPIO26;
        public static readonly int AdcChannelGp27 = Rp2350AdcLayout.Channel_GPIO27;
        public static readonly int AdcChannelGp28 = Rp2350AdcLayout.Channel_GPIO28;
        public static readonly int AdcChannelGp29 = Rp2350AdcLayout.Channel_GPIO29;
        public static readonly int AdcReferenceMicrovolts = (int)Pico2Bindings.ADC_REFERENCE_UV;

        /// <summary>The `adc` binding descriptor, lifted from the generated consts.</summary>
        public Rp2350AdcBinding CreateAdcBinding() { return AdcBinding(); }

        private static Rp2350AdcBinding AdcBinding()
        {
            return new Rp2350AdcBinding(
                Pico2Bindings.ADC_BASE,
                Pico2Bindings.ADC_RESET_MASK,
                Pico2Bindings.ADC_REFERENCE_UV);
        }

        /// <summary>An ADC controller over the RP2350 SAR converter (the on-chip temperature
        /// sensor is on <see cref="TemperatureSensorChannel"/>).</summary>
        public AdcController CreateAdcController()
        {
            return new AdcController();
        }

        /// <summary>The `uart0` binding descriptor (GP0 TX / GP1 RX, crystal-exact clk_peri).</summary>
        public Rp2350UartBinding CreateUartBinding()
        {
            return new Rp2350UartBinding(
                Pico2Bindings.UART0_BASE,
                Pico2Bindings.UART0_RESET_MASK,
                Pico2Bindings.UART0_IO_TX_CTRL,
                Pico2Bindings.UART0_IO_RX_CTRL,
                Pico2Bindings.UART0_PADS_TX,
                Pico2Bindings.UART0_PADS_RX,
                Pico2Bindings.UART0_FUNCSEL,
                Pico2Bindings.UART0_CLK_PERI_HZ);
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
                Pico2Bindings.SPI0_BASE,
                Pico2Bindings.SPI0_RESET_MASK,
                Pico2Bindings.SPI0_IO_MISO_CTRL,
                Pico2Bindings.SPI0_PADS_MISO,
                Pico2Bindings.SPI0_IO_CS_CTRL,
                Pico2Bindings.SPI0_PADS_CS,
                Pico2Bindings.SPI0_IO_SCK_CTRL,
                Pico2Bindings.SPI0_PADS_SCK,
                Pico2Bindings.SPI0_IO_MOSI_CTRL,
                Pico2Bindings.SPI0_PADS_MOSI,
                Pico2Bindings.SPI0_FUNCSEL,
                Pico2Bindings.SPI0_SSPCLK_HZ);
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
                Pico2Bindings.I2C0_BASE,
                Pico2Bindings.I2C0_RESET_MASK,
                Pico2Bindings.I2C0_IO_SDA_CTRL,
                Pico2Bindings.I2C0_PADS_SDA,
                Pico2Bindings.I2C0_IO_SCL_CTRL,
                Pico2Bindings.I2C0_PADS_SCL,
                Pico2Bindings.I2C0_FUNCSEL,
                Pico2Bindings.I2C0_IC_CLK_HZ);
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
            return raw * Pico2Bindings.ADC_REFERENCE_UV / fullScaleCounts;
        }
    }
}
