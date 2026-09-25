// Lamella.Boards.Microchip.Samd21Xpro -- the plain SAM D21 Xplained Pro (ATSAMD21J18A). Its EDBG VCP is
using System.Device.Analog;
using System.Device.Gpio;
using System.Device.Pwm;
using Lamella.Generated;
using Lamella.Hardware;

namespace Lamella.Boards.Microchip
{
    public sealed class Samd21Xpro
    {
        /// <summary>The wire identity this board advertises (lamella_wire::product_model).</summary>
        public static readonly int BoardModel = MicrochipSamd21XproBindings.BOARD_MODEL;

        /// <summary>Binds this board's GPIO block to the driver table, so a program writes plain
        /// dotnet/iot -- <c>new GpioController()</c> -- and never names a Lamella type. Touching
        /// <see cref="Samd21Xpro"/> at all is what arms it, which is why a program constructs the
        /// board first.</summary>
        /// <remarks>A TYPE INITIALIZER rather than the instance constructor, for the reason
        /// <see cref="Lamella.Hardware.Buses.BindGpio"/> documents: the table refuses a second bind
        /// of the same kind rather than replacing it, and this class is instantiable and routinely
        /// constructed as a temporary. The language runs a type initializer once per program, so
        /// idempotence costs nothing and the table keeps its throw as a genuine-error detector.
        /// The bound value is a FACTORY, not a driver, so a program that never touches GPIO never
        /// constructs one.</remarks>
        static Samd21Xpro()
        {
            Buses.BindGpio(new GpioDriverFactory(MakeGpio));
            Buses.BindSpi(Ext1SpiBusId, new SpiDriverFactory(MakeExt1Spi));
            Buses.BindI2c(ExtTwiBusId, new I2cDriverFactory(MakeExtTwi));
            AdcControllers.Bind(new AdcDriverFactory(MakeAdc));
#if LAMELLA_SURFACE_FLOAT
            Buses.BindPwm(PwmChipExt1, new PwmChannelFactory(OpenExt1Pwm));
            Buses.BindPwm(PwmChipExt2, new PwmChannelFactory(OpenExt2Pwm));
            Buses.BindPwm(PwmChipExt3, new PwmChannelFactory(OpenExt3Pwm));
#endif
        }

        /// <summary>The EXT1 header's PWM chip, as <c>PwmChannel.Create</c> names it: channel 0 on
        /// pin 7 (PB02) and channel 1 on pin 8 (PB03), the two outputs of the 8-bit counter TC6. They
        /// share its rate, from 31 Hz to 4 MHz.</summary>
        public const int PwmChipExt1 = 0;
        /// <summary>The EXT2 header's PWM chip: channel 0 on pin 7 (PB12) and channel 1 on pin 8
        /// (PB13), the two outputs of the 8-bit counter TC4. They share its rate, from 31 Hz to
        /// 4 MHz.</summary>
        public const int PwmChipExt2 = 1;
        /// <summary>The EXT3 header's PWM chip: channel 0 on pin 7 (PA12), an output of the 16-bit
        /// counter TCC2, from 1 Hz to 4 MHz. Pin 8 is not a channel: its pad, PA13, is the serial
        /// flash's chip select, and reaches the header only with the resistor R314 moved to
        /// R313.</summary>
        public const int PwmChipExt3 = 2;

        /// <summary>The `ext1-pwm` binding descriptor -- TC6 and the EXT1 header's two PWM pins --
        /// lifted from the generated constants.</summary>
        public Samd21PwmBinding CreateExt1PwmBinding() { return Ext1PwmBinding(); }

        /// <summary>The `ext2-pwm` binding descriptor -- TC4 and the EXT2 header's two PWM pins --
        /// lifted from the generated constants.</summary>
        public Samd21PwmBinding CreateExt2PwmBinding() { return Ext2PwmBinding(); }

        /// <summary>The `ext3-pwm` binding descriptor -- TCC2 and the EXT3 header's PWM pin --
        /// lifted from the generated constants.</summary>
        public Samd21PwmBinding CreateExt3PwmBinding() { return Ext3PwmBinding(); }

        private static Samd21PwmBinding Ext1PwmBinding()
        {
            return new Samd21PwmBinding(
                MicrochipSamd21XproBindings.EXT1_PWM_BASE,
                MicrochipSamd21XproBindings.EXT1_PWM_GCLK_CLKCTRL_VALUE,
                MicrochipSamd21XproBindings.EXT1_PWM_APBC_MASK,
                MicrochipSamd21XproBindings.EXT1_PWM_CORE_CLOCK_HZ,
                MicrochipSamd21XproBindings.EXT1_PWM_COUNTER_BITS,
                MicrochipSamd21XproBindings.EXT1_PWM_PMUX_FUNC,
                new Samd21PwmOutput[] {
                    new Samd21PwmOutput((int)MicrochipSamd21XproBindings.EXT1_PWM_CC_WO0,
                        MicrochipSamd21XproBindings.EXT1_PWM_PMUX_WO0_REG,
                        MicrochipSamd21XproBindings.EXT1_PWM_PMUX_WO0_SHIFT,
                        MicrochipSamd21XproBindings.EXT1_PWM_PINCFG_WO0_REG),
                    new Samd21PwmOutput((int)MicrochipSamd21XproBindings.EXT1_PWM_CC_WO1,
                        MicrochipSamd21XproBindings.EXT1_PWM_PMUX_WO1_REG,
                        MicrochipSamd21XproBindings.EXT1_PWM_PMUX_WO1_SHIFT,
                        MicrochipSamd21XproBindings.EXT1_PWM_PINCFG_WO1_REG)
                });
        }

        private static Samd21PwmBinding Ext2PwmBinding()
        {
            return new Samd21PwmBinding(
                MicrochipSamd21XproBindings.EXT2_PWM_BASE,
                MicrochipSamd21XproBindings.EXT2_PWM_GCLK_CLKCTRL_VALUE,
                MicrochipSamd21XproBindings.EXT2_PWM_APBC_MASK,
                MicrochipSamd21XproBindings.EXT2_PWM_CORE_CLOCK_HZ,
                MicrochipSamd21XproBindings.EXT2_PWM_COUNTER_BITS,
                MicrochipSamd21XproBindings.EXT2_PWM_PMUX_FUNC,
                new Samd21PwmOutput[] {
                    new Samd21PwmOutput((int)MicrochipSamd21XproBindings.EXT2_PWM_CC_WO0,
                        MicrochipSamd21XproBindings.EXT2_PWM_PMUX_WO0_REG,
                        MicrochipSamd21XproBindings.EXT2_PWM_PMUX_WO0_SHIFT,
                        MicrochipSamd21XproBindings.EXT2_PWM_PINCFG_WO0_REG),
                    new Samd21PwmOutput((int)MicrochipSamd21XproBindings.EXT2_PWM_CC_WO1,
                        MicrochipSamd21XproBindings.EXT2_PWM_PMUX_WO1_REG,
                        MicrochipSamd21XproBindings.EXT2_PWM_PMUX_WO1_SHIFT,
                        MicrochipSamd21XproBindings.EXT2_PWM_PINCFG_WO1_REG)
                });
        }

        private static Samd21PwmBinding Ext3PwmBinding()
        {
            return new Samd21PwmBinding(
                MicrochipSamd21XproBindings.EXT3_PWM_BASE,
                MicrochipSamd21XproBindings.EXT3_PWM_GCLK_CLKCTRL_VALUE,
                MicrochipSamd21XproBindings.EXT3_PWM_APBC_MASK,
                MicrochipSamd21XproBindings.EXT3_PWM_CORE_CLOCK_HZ,
                MicrochipSamd21XproBindings.EXT3_PWM_COUNTER_BITS,
                MicrochipSamd21XproBindings.EXT3_PWM_PMUX_FUNC,
                new Samd21PwmOutput[] {
                    new Samd21PwmOutput((int)MicrochipSamd21XproBindings.EXT3_PWM_CC_WO0,
                        MicrochipSamd21XproBindings.EXT3_PWM_PMUX_WO0_REG,
                        MicrochipSamd21XproBindings.EXT3_PWM_PMUX_WO0_SHIFT,
                        MicrochipSamd21XproBindings.EXT3_PWM_PINCFG_WO0_REG)
                });
        }

#if LAMELLA_SURFACE_FLOAT
        private static Samd21TcPwm _ext1Pwm;
        private static Samd21TcPwm _ext2Pwm;
        private static Samd21TccPwm _ext3Pwm;

        private static PwmChannel OpenExt1Pwm(int channel, int frequency, double dutyCyclePercentage)
        {
            if ((object)_ext1Pwm == null)
            {
                _ext1Pwm = new Samd21TcPwm(Ext1PwmBinding());
            }
            return _ext1Pwm.Open(channel, frequency, dutyCyclePercentage);
        }

        private static PwmChannel OpenExt2Pwm(int channel, int frequency, double dutyCyclePercentage)
        {
            if ((object)_ext2Pwm == null)
            {
                _ext2Pwm = new Samd21TcPwm(Ext2PwmBinding());
            }
            return _ext2Pwm.Open(channel, frequency, dutyCyclePercentage);
        }

        private static PwmChannel OpenExt3Pwm(int channel, int frequency, double dutyCyclePercentage)
        {
            if ((object)_ext3Pwm == null)
            {
                _ext3Pwm = new Samd21TccPwm(Ext3PwmBinding());
            }
            return _ext3Pwm.Open(channel, frequency, dutyCyclePercentage);
        }
#endif

        /// <summary>The logical bus id of the TWI all three extension headers share (SDA PA08 and
        /// SCL PA09, on pins 11 and 12 of each), as <c>I2cConnectionSettings</c> names it. The kit's
        /// debugger reaches the same two pads.</summary>
        public const int ExtTwiBusId = 0;

        private static I2cDriver MakeExtTwi() { return new Samd21I2cDriver(ExtTwiBinding()); }

        /// <summary>The `ext-twi` binding descriptor -- the extension headers' shared TWI -- lifted
        /// from the generated constants. The bus speed is not in it: that is a runtime
        /// <c>Configure</c> choice, derived from the core-clock rate.</summary>
        public Samd21SercomI2cBinding CreateExtTwiBinding() { return ExtTwiBinding(); }

        private static Samd21SercomI2cBinding ExtTwiBinding()
        {
            return new Samd21SercomI2cBinding(
                MicrochipSamd21XproBindings.EXT_TWI_SERCOM_BASE,
                MicrochipSamd21XproBindings.EXT_TWI_GCLK_CLKCTRL_VALUE,
                MicrochipSamd21XproBindings.EXT_TWI_APBC_MASK,
                MicrochipSamd21XproBindings.EXT_TWI_PMUX_REG,
                MicrochipSamd21XproBindings.EXT_TWI_PMUX_PAIR,
                MicrochipSamd21XproBindings.EXT_TWI_PINCFG_SDA_REG,
                MicrochipSamd21XproBindings.EXT_TWI_PINCFG_SCL_REG,
                MicrochipSamd21XproBindings.EXT_TWI_CORE_CLOCK_HZ);
        }

        /// <summary>The extension headers' TWI as the layer-1 driver the board's table binds for
        /// <see cref="ExtTwiBusId"/>, not yet configured.</summary>
        /// <remarks>THE SAME INSTANCE <c>I2cDevice.Create</c> uses for that bus, for the reason
        /// <see cref="Lamella.Hardware.Buses.ResolveSpi"/> gives.</remarks>
        public I2cDriver CreateExtTwiDriver()
        {
            return Buses.ResolveI2c(ExtTwiBusId);
        }

        private static GpioDriver MakeGpio() { return new Samd21GpioDriver(); }
        private static AdcDriver MakeAdc() { return new Samd21AdcDriver(AdcBinding()); }


        /// <summary>The converter channel on EXT1 pin 3, ADC(+): PB00, AIN8.</summary>
        public static readonly int AdcChannelExt1Pin3 = (int)MicrochipSamd21XproBindings.ADC_MUXPOS_EXT1_PIN3;
        /// <summary>The converter channel on EXT1 pin 4, ADC(-): PB01, AIN9.</summary>
        public static readonly int AdcChannelExt1Pin4 = (int)MicrochipSamd21XproBindings.ADC_MUXPOS_EXT1_PIN4;
        /// <summary>The converter channel on EXT2 pin 3, ADC(+): PA10, AIN18.</summary>
        public static readonly int AdcChannelExt2Pin3 = (int)MicrochipSamd21XproBindings.ADC_MUXPOS_EXT2_PIN3;
        /// <summary>The converter channel on EXT2 pin 4, ADC(-): PA11, AIN19.</summary>
        public static readonly int AdcChannelExt2Pin4 = (int)MicrochipSamd21XproBindings.ADC_MUXPOS_EXT2_PIN4;
        /// <summary>The converter channel on EXT3 pin 3, ADC(+): PA02, AIN0.</summary>
        public static readonly int AdcChannelExt3Pin3 = (int)MicrochipSamd21XproBindings.ADC_MUXPOS_EXT3_PIN3;
        /// <summary>The converter channel on EXT3 pin 4, ADC(-): PA03, AIN1. The pad is also the
        /// USB ID line, and reaches the header only with jumper JS300 set for it.</summary>
        public static readonly int AdcChannelExt3Pin4 = (int)MicrochipSamd21XproBindings.ADC_MUXPOS_EXT3_PIN4;
        /// <summary>The internal bandgap reference as a converter channel: typically 1.1 V
        /// (DS40001882D Table 37-38).</summary>
        public static readonly int AdcChannelBandgap = Samd21AdcLayout.Channel_Bandgap;
        /// <summary>The core supply scaled by a quarter, as a converter channel.</summary>
        public static readonly int AdcChannelScaledCoreSupply = Samd21AdcLayout.Channel_ScaledCoreSupply;
        /// <summary>The I/O supply scaled by a quarter, as a converter channel.</summary>
        public static readonly int AdcChannelScaledIoSupply = Samd21AdcLayout.Channel_ScaledIoSupply;
        /// <summary>What a full-scale count means, in microvolts: the board's 3.3 V analog supply.</summary>
        public static readonly int AdcReferenceMicrovolts = (int)MicrochipSamd21XproBindings.ADC_REFERENCE_UV;

        /// <summary>The `adc` binding descriptor -- the converter and the six analog pads of the
        /// extension headers -- lifted from the generated constants.</summary>
        public Samd21AdcBinding CreateAdcBinding() { return AdcBinding(); }

        private static Samd21AdcBinding AdcBinding()
        {
            return new Samd21AdcBinding(
                MicrochipSamd21XproBindings.ADC_BASE,
                MicrochipSamd21XproBindings.ADC_GCLK_CLKCTRL_VALUE,
                MicrochipSamd21XproBindings.ADC_APBC_MASK,
                MicrochipSamd21XproBindings.ADC_PRESCALER,
                MicrochipSamd21XproBindings.ADC_PMUX_FUNC,
                MicrochipSamd21XproBindings.ADC_REFERENCE_UV,
                new Samd21AdcPad[] {
                    new Samd21AdcPad((int)MicrochipSamd21XproBindings.ADC_MUXPOS_EXT1_PIN3,
                        MicrochipSamd21XproBindings.ADC_PMUX_EXT1_PIN3_REG,
                        MicrochipSamd21XproBindings.ADC_PMUX_EXT1_PIN3_SHIFT,
                        MicrochipSamd21XproBindings.ADC_PINCFG_EXT1_PIN3_REG),
                    new Samd21AdcPad((int)MicrochipSamd21XproBindings.ADC_MUXPOS_EXT1_PIN4,
                        MicrochipSamd21XproBindings.ADC_PMUX_EXT1_PIN4_REG,
                        MicrochipSamd21XproBindings.ADC_PMUX_EXT1_PIN4_SHIFT,
                        MicrochipSamd21XproBindings.ADC_PINCFG_EXT1_PIN4_REG),
                    new Samd21AdcPad((int)MicrochipSamd21XproBindings.ADC_MUXPOS_EXT2_PIN3,
                        MicrochipSamd21XproBindings.ADC_PMUX_EXT2_PIN3_REG,
                        MicrochipSamd21XproBindings.ADC_PMUX_EXT2_PIN3_SHIFT,
                        MicrochipSamd21XproBindings.ADC_PINCFG_EXT2_PIN3_REG),
                    new Samd21AdcPad((int)MicrochipSamd21XproBindings.ADC_MUXPOS_EXT2_PIN4,
                        MicrochipSamd21XproBindings.ADC_PMUX_EXT2_PIN4_REG,
                        MicrochipSamd21XproBindings.ADC_PMUX_EXT2_PIN4_SHIFT,
                        MicrochipSamd21XproBindings.ADC_PINCFG_EXT2_PIN4_REG),
                    new Samd21AdcPad((int)MicrochipSamd21XproBindings.ADC_MUXPOS_EXT3_PIN3,
                        MicrochipSamd21XproBindings.ADC_PMUX_EXT3_PIN3_REG,
                        MicrochipSamd21XproBindings.ADC_PMUX_EXT3_PIN3_SHIFT,
                        MicrochipSamd21XproBindings.ADC_PINCFG_EXT3_PIN3_REG),
                    new Samd21AdcPad((int)MicrochipSamd21XproBindings.ADC_MUXPOS_EXT3_PIN4,
                        MicrochipSamd21XproBindings.ADC_PMUX_EXT3_PIN4_REG,
                        MicrochipSamd21XproBindings.ADC_PMUX_EXT3_PIN4_SHIFT,
                        MicrochipSamd21XproBindings.ADC_PINCFG_EXT3_PIN4_REG)
                });
        }

        /// <summary>The on-chip converter as dotnet/iot's analog controller. Each pin number is a
        /// converter channel: <see cref="AdcChannelExt1Pin3"/> to <see cref="AdcChannelExt3Pin4"/>
        /// name the six header pads, and <see cref="AdcChannelBandgap"/>,
        /// <see cref="AdcChannelScaledCoreSupply"/> and <see cref="AdcChannelScaledIoSupply"/> the
        /// internal inputs.</summary>
        /// <remarks>Creating it touches no hardware; the first pin opened brings the converter up.</remarks>
        /// <param name="chip">Must be 0: the board has one converter.</param>
        /// <exception cref="System.ArgumentOutOfRangeException"><paramref name="chip"/> is not 0.</exception>
        public AnalogController CreateAnalogController(int chip)
        {
            if (chip != 0)
            {
                throw new System.ArgumentOutOfRangeException("chip");
            }
            return AdcControllers.CreateAnalogController();
        }

        /// <summary>The logical bus id of the EXT1 header's SPI (MOSI PA06, MISO PA04 and SCK PA07,
        /// on header pins 16, 17 and 18), as <c>SpiConnectionSettings</c> names it.</summary>
        public const int Ext1SpiBusId = 0;

        /// <summary>The EXT1 header's chip select line, SPI_SS_A on pin 15, as
        /// <c>SpiConnectionSettings.ChipSelectLine</c> takes it.</summary>
        public static readonly int Ext1SpiChipSelectLine = Samd21GpioDriver.LogicalPin(
            MicrochipSamd21XproBindings.EXT1_SPI_CS_PORT_BASE,
            MicrochipSamd21XproBindings.EXT1_SPI_CS_PIN);

        private static SpiDriver MakeExt1Spi() { return new Samd21SpiDriver(Ext1SpiBinding()); }

        /// <summary>The `ext1-spi` binding descriptor -- the EXT1 header's SPI -- lifted from the
        /// generated constants.</summary>
        public Samd21SercomSpiBinding CreateExt1SpiBinding() { return Ext1SpiBinding(); }

        private static Samd21SercomSpiBinding Ext1SpiBinding()
        {
            return new Samd21SercomSpiBinding(
                MicrochipSamd21XproBindings.EXT1_SPI_SERCOM_BASE,
                MicrochipSamd21XproBindings.EXT1_SPI_GCLK_CLKCTRL_VALUE,
                MicrochipSamd21XproBindings.EXT1_SPI_APBC_MASK,
                MicrochipSamd21XproBindings.EXT1_SPI_PMUX_MOSI_REG,
                MicrochipSamd21XproBindings.EXT1_SPI_PMUX_MOSI_SHIFT,
                MicrochipSamd21XproBindings.EXT1_SPI_PINCFG_MOSI_REG,
                MicrochipSamd21XproBindings.EXT1_SPI_PMUX_SCK_REG,
                MicrochipSamd21XproBindings.EXT1_SPI_PMUX_SCK_SHIFT,
                MicrochipSamd21XproBindings.EXT1_SPI_PINCFG_SCK_REG,
                MicrochipSamd21XproBindings.EXT1_SPI_PMUX_MISO_REG,
                MicrochipSamd21XproBindings.EXT1_SPI_PMUX_MISO_SHIFT,
                MicrochipSamd21XproBindings.EXT1_SPI_PINCFG_MISO_REG,
                MicrochipSamd21XproBindings.EXT1_SPI_PMUX_FUNC,
                MicrochipSamd21XproBindings.EXT1_SPI_DOPO,
                MicrochipSamd21XproBindings.EXT1_SPI_DIPO,
                MicrochipSamd21XproBindings.EXT1_SPI_CORE_CLOCK_HZ);
        }

        /// <summary>The EXT1 header's SPI as the layer-1 driver the board's table binds for
        /// <see cref="Ext1SpiBusId"/>, not yet configured.</summary>
        /// <remarks>THE SAME INSTANCE <c>SpiDevice.Create</c> uses for that bus, for the reason
        /// <see cref="Lamella.Hardware.Buses.ResolveSpi"/> gives.</remarks>
        public SpiDriver CreateExt1SpiDriver()
        {
            return Buses.ResolveSpi(Ext1SpiBusId);
        }

        /// <summary>The family PORT driver this board bound, over every pin on the part.</summary>
        /// <remarks>THE SAME INSTANCE <see cref="GpioController"/> drives. One block has one
        /// driver, and handing out a second one over the same registers reads as working while
        /// the facade talks to the first -- see
        /// <see cref="Lamella.Hardware.Buses.ResolveSpi"/> for the full argument.</remarks>
        public GpioDriver CreateGpioDriver()
        {
            return Buses.ResolveGpio();
        }

        /// <summary>The EDBG virtual-COM UART (SERCOM3, PA22 TX / PA23 RX, 115200-8N1 under
        /// the osc8m-8mhz plan), ready for <c>Init()</c>.</summary>
        public Samd21Uart CreateVcpUart()
        {
            return new Samd21Uart(new Samd21SercomUsartBinding(
                MicrochipSamd21XproBindings.VCP_SERCOM_BASE,
                MicrochipSamd21XproBindings.VCP_GCLK_CLKCTRL_VALUE,
                MicrochipSamd21XproBindings.VCP_APBC_MASK,
                MicrochipSamd21XproBindings.VCP_PMUX_REG,
                MicrochipSamd21XproBindings.VCP_PMUX_PAIR,
                MicrochipSamd21XproBindings.VCP_PINCFG_TX_REG,
                MicrochipSamd21XproBindings.VCP_PINCFG_RX_REG,
                MicrochipSamd21XproBindings.VCP_TXPO,
                MicrochipSamd21XproBindings.VCP_RXPO,
                MicrochipSamd21XproBindings.VCP_BAUD_115200_OSC8M_8MHZ));
        }
    }
}
