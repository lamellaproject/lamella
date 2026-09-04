// Lamella.Boards.Microchip.Same54Xpro -- the SAM E54 Xplained Pro (ATSAME54P20A).
using System.Device.Gpio;
using Lamella.Generated;
using Lamella.Hardware;

namespace Lamella.Boards.Microchip
{
    public sealed class Same54Xpro
    {
        /// <summary>The wire identity this board advertises (lamella_wire::board_model).</summary>
        public static readonly int BoardModel = Same54XproBindings.BOARD_MODEL;

        /// <summary>The yellow user LED LD0, ACTIVE LOW: it lights when driven
        /// <see cref="PinValue.Low"/>.</summary>
        /// <remarks>The pad is shared with the position decoder and with one of the embedded
        /// debugger's GPIOs, so a design using either reaches this same wire.</remarks>
        public static readonly int LedPin =
            Same54GpioDriver.LogicalPin(Same54XproBindings.LED0_PORT_BASE, Same54XproBindings.LED0_PIN);

        /// <summary>The SW0 user button, ACTIVE LOW: pressing it drives the line to ground, so a
        /// pressed button reads <see cref="PinValue.Low"/>.</summary>
        /// <remarks>OPEN IT WITH <see cref="ButtonMode"/>. There is no pull-up resistor on this
        /// board, so a plain input floats.</remarks>
        public static readonly int ButtonPin =
            Same54GpioDriver.LogicalPin(Same54XproBindings.BUTTON0_PORT_BASE, Same54XproBindings.BUTTON0_PIN);

        /// <summary>The mode the user button needs: the board provides no pull-up, so the internal
        /// one carries the line while the button is not pressed.</summary>
        public static readonly PinMode ButtonMode = PinMode.InputPullUp;

        /// <summary>Binds this board's GPIO block to the driver table, so a program writes plain
        /// dotnet/iot -- <c>new GpioController()</c> -- and never names a Lamella type. Touching
        /// <see cref="Same54Xpro"/> at all is what arms it, which is why a program constructs the
        /// board first.</summary>
        /// <remarks>A TYPE INITIALIZER rather than the instance constructor, for the reason
        /// <see cref="Lamella.Hardware.Buses.BindGpio"/> documents: the table refuses a second bind
        /// of the same kind rather than replacing it, and this class is instantiable and routinely
        /// constructed as a temporary. The language runs a type initializer once per program, so
        /// idempotence costs nothing and the table keeps its throw as a genuine-error detector.
        /// The bound value is a FACTORY, not a driver, so a program that never touches GPIO never
        /// constructs one.</remarks>
        static Same54Xpro()
        {
            Buses.BindGpio(new GpioDriverFactory(MakeGpio));
        }

        private static GpioDriver MakeGpio() { return new Same54GpioDriver(); }

        /// <summary>The family GPIO driver this board bound, over PA00..PD31.</summary>
        /// <remarks>THE SAME INSTANCE <see cref="GpioController"/> drives. One block has one
        /// driver, and handing out a second one over the same registers reads as working while the
        /// facade talks to the first -- see <see cref="Lamella.Hardware.Buses.ResolveSpi"/> for the
        /// full argument.</remarks>
        public GpioDriver CreateGpioDriver()
        {
            return Buses.ResolveGpio();
        }

        /// <summary>A GPIO controller over the SAM E54 PORT block.</summary>
        public GpioController CreateGpioController()
        {
            return new GpioController();
        }

        /// <summary>The I2C master this board offers on EXT1 (SERCOM3 on PA22/PA23), as a
        /// descriptor. The bus SPEED is not here: it is a runtime <c>Configure</c> choice derived
        /// on the device from the core-clock rate.</summary>
        /// <remarks>Every value is a generated literal, so this method holds no address, no mask
        /// and no divisor of its own -- and the two clock fields are REGISTER ADDRESSES rather than
        /// a composed word and a bare bit, because this family routes each core clock through its
        /// own channel register and gates its SERCOMs from three different APB masks.</remarks>
        public Same54SercomI2cBinding Ext1I2cBinding()
        {
            return new Same54SercomI2cBinding(
                Same54XproBindings.EXT1_I2C_SERCOM_BASE,
                Same54XproBindings.EXT1_I2C_GCLK_PCHCTRL_REG,
                Same54XproBindings.EXT1_I2C_GCLK_PCHCTRL_VALUE,
                Same54XproBindings.EXT1_I2C_APB_MASK_REG,
                Same54XproBindings.EXT1_I2C_APB_MASK,
                Same54XproBindings.EXT1_I2C_PMUX_REG,
                Same54XproBindings.EXT1_I2C_PMUX_PAIR,
                Same54XproBindings.EXT1_I2C_PINCFG_SDA_REG,
                Same54XproBindings.EXT1_I2C_PINCFG_SCL_REG,
                Same54XproBindings.EXT1_I2C_CORE_CLOCK_HZ);
        }

        /// <summary>An I2C master driver over EXT1, unconfigured. The caller calls
        /// <c>Configure</c> with the wire rate it wants.</summary>
        public I2cDriver CreateExt1I2cDriver()
        {
            return new Same54I2cDriver(Ext1I2cBinding());
        }

        /// <summary>The analog input this board offers on EXT1 (ADC1's AIN6 on PB04), as a
        /// descriptor.</summary>
        /// <remarks>The reference is this board's own 3.3V analog supply, and the driver selects
        /// that supply as the converter's reference -- so the voltage here and the one the hardware
        /// uses are the same rail by construction rather than by coincidence.</remarks>
        public Same54AdcBinding Ext1AdcBinding()
        {
            return new Same54AdcBinding(
                Same54XproBindings.EXT1_ADC_P_ADC_BASE,
                Same54XproBindings.EXT1_ADC_P_GCLK_PCHCTRL_REG,
                Same54XproBindings.EXT1_ADC_P_GCLK_PCHCTRL_VALUE,
                Same54XproBindings.EXT1_ADC_P_APB_MASK_REG,
                Same54XproBindings.EXT1_ADC_P_APB_MASK,
                Same54XproBindings.EXT1_ADC_P_CALIB_REG,
                Same54XproBindings.EXT1_ADC_P_NVM_CALIB_AREA,
                Same54XproBindings.EXT1_ADC_P_NVM_CALIB_LSB,
                Same54XproBindings.EXT1_ADC_P_PMUX_REG,
                Same54XproBindings.EXT1_ADC_P_PMUX_MASK,
                Same54XproBindings.EXT1_ADC_P_PMUX_VALUE,
                Same54XproBindings.EXT1_ADC_P_PINCFG_REG,
                Same54XproBindings.EXT1_ADC_P_MUXPOS,
                Same54XproBindings.EXT1_ADC_P_REFERENCE_UV);
        }

        /// <summary>An ADC driver over EXT1's analog pad, configured and ready to read.</summary>
        /// <remarks>ONE CHANNEL, and channel 0 is that pad. The converter has sixteen inputs and
        /// this board has handed one pad to the analog function, so the other fifteen would sample
        /// pins the PORT still owns -- a number with no error, meaning nothing.</remarks>
        public AdcDriver CreateExt1AdcDriver()
        {
            return new Same54AdcDriver(Ext1AdcBinding());
        }
    }
}
