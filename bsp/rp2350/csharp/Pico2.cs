// Lamella.Boards.Pico2 -- the Raspberry Pi Pico 2 / Pico 2 W (RP2350) board-support package.
using System.Device.Adc;
using System.Device.Gpio;
using System.Device.I2c;
using System.Device.Spi;
using Lamella.Generated;
using Lamella.Hardware;

namespace Lamella.Boards
{
    public sealed class Pico2
    {
        /// <summary>Constructs the board and ensures its clock tree is up (idempotent) -- so
        /// `new Pico2()` "just works" on both tiers: the interpreter serve already raised clocks
        /// at boot (guard no-op), an AOT image raises them here.</summary>
        public Pico2()
        {
            EnsureClocks();
        }


        /// <summary>Brings the clock tree up if it is not already running -- idempotent. The no-op
        /// guard is CONJUNCTIVE (clk_sys on PLL_SYS AND both PLLs locked), so a PARTIAL state (e.g.
        /// after a soft restart: clk_sys on PLL_SYS but a PLL unlocked) takes the FULL bring-up,
        /// which parks clk_sys on clk_ref FIRST so re-cycling PLL_SYS never kills the clock the core
        /// is running on. Public so a boot stub or a test can call it explicitly.</summary>
        public static void EnsureClocks()
        {
            bool sysOnPll = (Mmio.Read32(Rp2350ClockFacts.CLK_SYS_SELECTED) & Rp2350ClockFacts.CLK_SYS_AUX_SELECTED) != 0u;
            bool sysLocked = (Mmio.Read32(Rp2350ClockFacts.PLL_SYS_CS) & Rp2350ClockFacts.PLL_LOCK) != 0u;
            bool usbLocked = (Mmio.Read32(Rp2350ClockFacts.PLL_USB_CS) & Rp2350ClockFacts.PLL_LOCK) != 0u;
            if (sysOnPll && sysLocked && usbLocked) return;

            Mmio.Write32(Rp2350ClockFacts.XOSC_STARTUP, Rp2350ClockFacts.XOSC_STARTUP_DELAY);
            Mmio.Write32(Rp2350ClockFacts.XOSC_CTRL, Rp2350ClockFacts.XOSC_ENABLE_1_15MHZ);
            for (int spin = 0; spin < 1000000; spin++)
            {
                if ((Mmio.Read32(Rp2350ClockFacts.XOSC_STATUS) & Rp2350ClockFacts.XOSC_STATUS_STABLE) != 0u) break;
            }

            Mmio.Write32(Rp2350ClockFacts.CLK_REF_CTRL, Rp2350ClockFacts.CLK_REF_SRC_XOSC);
            for (int spin = 0; spin < 100000; spin++)
            {
                if ((Mmio.Read32(Rp2350ClockFacts.CLK_REF_SELECTED) & Rp2350ClockFacts.CLK_REF_XOSC_SELECTED) != 0u) break;
            }
            Mmio.Write32(Rp2350ClockFacts.CLK_SYS_CTRL, 0);
            for (int spin = 0; spin < 100000; spin++)
            {
                if ((Mmio.Read32(Rp2350ClockFacts.CLK_SYS_SELECTED) & Rp2350ClockFacts.CLK_SYS_REF_SELECTED) != 0u) break;
            }

            InitPll(Rp2350ClockFacts.PLL_SYS_CS, Rp2350ClockFacts.PLL_SYS_FBDIV_INT, Rp2350ClockFacts.PLL_SYS_PRIM,
                Rp2350ClockFacts.PLL_SYS_PWR_CLR, Rp2350ClockFacts.RESETS_DONE_PLL_SYS,
                Rp2350ClockFacts.PLL_SYS_FBDIV, Rp2350ClockFacts.PLL_SYS_POSTDIVS);
            Mmio.Write32(Rp2350ClockFacts.CLK_SYS_CTRL, 0);
            Mmio.Write32(Rp2350ClockFacts.CLK_SYS_CTRL, Rp2350ClockFacts.CLK_SYS_SRC_AUX);
            for (int spin = 0; spin < 100000; spin++)
            {
                if ((Mmio.Read32(Rp2350ClockFacts.CLK_SYS_SELECTED) & Rp2350ClockFacts.CLK_SYS_AUX_SELECTED) != 0u) break;
            }

            InitPll(Rp2350ClockFacts.PLL_USB_CS, Rp2350ClockFacts.PLL_USB_FBDIV_INT, Rp2350ClockFacts.PLL_USB_PRIM,
                Rp2350ClockFacts.PLL_USB_PWR_CLR, Rp2350ClockFacts.RESETS_DONE_PLL_USB,
                Rp2350ClockFacts.PLL_USB_FBDIV, Rp2350ClockFacts.PLL_USB_POSTDIVS);
            Mmio.Write32(Rp2350ClockFacts.CLK_USB_CTRL, 0);
            Mmio.Write32(Rp2350ClockFacts.CLK_USB_CTRL, Rp2350ClockFacts.CLK_USB_ENABLE_BIT);
        }

        static void InitPll(uint cs, uint fbdiv, uint prim, uint pwrClr, uint resetBit, uint fbdivValue, uint primValue)
        {
            ResetCycle(resetBit);
            Mmio.Write32(cs, Rp2350ClockFacts.PLL_REFDIV_1);
            Mmio.Write32(fbdiv, fbdivValue);
            Mmio.Write32(pwrClr, Rp2350ClockFacts.PLL_PWR_PD_VCOPD);
            for (int spin = 0; spin < 1000000; spin++)
            {
                if ((Mmio.Read32(cs) & Rp2350ClockFacts.PLL_LOCK) != 0u) break;
            }
            Mmio.Write32(prim, primValue);
            Mmio.Write32(pwrClr, Rp2350ClockFacts.PLL_PWR_POSTDIVPD);
        }

        static void ResetCycle(uint resetBit)
        {
            Mmio.Write32(Rp2350ClockFacts.RESETS_SET, resetBit);
            for (int spin = 0; spin < 100000; spin++)
            {
                if ((Mmio.Read32(Rp2350ClockFacts.RESETS_DONE) & resetBit) == 0u) break;
            }
            Mmio.Write32(Rp2350ClockFacts.RESETS_CLR, resetBit);
            for (int spin = 0; spin < 100000; spin++)
            {
                if ((Mmio.Read32(Rp2350ClockFacts.RESETS_DONE) & resetBit) != 0u) break;
            }
        }

        public static readonly int TemperatureSensorChannel = Rp2350AdcFacts.Channel_TemperatureSensor;
        public static readonly int AdcChannelGp26 = Rp2350AdcFacts.Channel_GPIO26;
        public static readonly int AdcChannelGp27 = Rp2350AdcFacts.Channel_GPIO27;
        public static readonly int AdcChannelGp28 = Rp2350AdcFacts.Channel_GPIO28;
        public static readonly int AdcChannelGp29 = Rp2350AdcFacts.Channel_GPIO29;
        public static readonly int AdcReferenceMicrovolts = (int)Rp2350AdcFacts.ReferenceUv;

        /// <summary>An ADC controller over the RP2350 SAR converter (the on-chip temperature
        /// sensor is on <see cref="TemperatureSensorChannel"/>).</summary>
        public AdcController CreateAdcController()
        {
            return new AdcController(new Rp2350AdcDriver());
        }

        /// <summary>A SPI device on SPI0 (GP18 SCK / GP19 MOSI / GP16 MISO). A negative
        /// <paramref name="chipSelectLine"/> routes GP17 as the PL022's hardware CS; a
        /// non-negative line is driven as a managed SIO chip-select.</summary>
        public SpiDevice CreateSpiDevice(int chipSelectLine)
        {
            return SpiDevice.Create(new SpiConnectionSettings(0, chipSelectLine), new Rp2350SpiDriver());
        }

        /// <summary>An I2C device on I2C0 (GP4 SDA / GP5 SCL) at <paramref name="deviceAddress"/>.</summary>
        public I2cDevice CreateI2cDevice(int deviceAddress)
        {
            return I2cDevice.Create(new I2cConnectionSettings(0, deviceAddress), new Rp2350I2cDriver());
        }

        /// <summary>A GPIO controller over the RP2350 SIO/pad block.</summary>
        public GpioController CreateGpioController()
        {
            return new GpioController(new Rp2350GpioDriver());
        }

        /// <summary>Converts a temperature-sensor microvolt reading to milli-degrees Celsius using
        /// the GENERATED vbe-linear calibration record (adc-rp2350.toml): the integer form both
        /// language skins share, T_mC = t0 + (v_uV - vbe) * 1000 / slope (truncating; slope is
        /// negative, so rising voltage falls in temperature). Demos call this instead of inlining
        /// the coefficients.</summary>
        public static int TemperatureMilliCelsius(long microvolts)
        {
            long t0 = Rp2350AdcFacts.TemperatureSensor_T0Millicelsius;
            long vbe = Rp2350AdcFacts.TemperatureSensor_VbeAtT0Microvolts;
            long slope = Rp2350AdcFacts.TemperatureSensor_SlopeMicrovoltsPerCelsius;
            return (int)(t0 + (microvolts - vbe) * 1000L / slope);
        }

        /// <summary>Microvolts from a raw ADC count, using the generated board reference voltage
        /// over <paramref name="fullScaleCounts"/> (truncating). For an averaged sum, pass the
        /// sum as <paramref name="raw"/> and counts * samples as the scale.</summary>
        public static long MicrovoltsFromRaw(long raw, long fullScaleCounts)
        {
            return raw * Rp2350AdcFacts.ReferenceUv / fullScaleCounts;
        }
    }
}
