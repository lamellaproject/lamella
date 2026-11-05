// Lamella.Hardware.PinFunctions -- what a pin of the board's chip can be.
namespace Lamella.Hardware
{
    /// <summary>What a pin can be: the chip's own function table where the board's facts hold it, and
    /// otherwise GPIO and the functions the board binds to the pin. A pin carries every function whose
    /// flag is set.</summary>
    [System.Flags]
    public enum PinFunctions
    {
        /// <summary>No function: the number names no pin the board's chip brings out.</summary>
        None = 0,

        /// <summary>A general-purpose input or output, the pin <c>GpioController.OpenPin</c> opens by
        /// its number. A board that keeps a pin for a part of its own, as a radio or a PSRAM holds its
        /// lines, refuses a mode on that pin.</summary>
        Gpio = 1,

        /// <summary>The data line of an I2C bus.</summary>
        I2cSda = 2,

        /// <summary>The clock line of an I2C bus.</summary>
        I2cScl = 4,

        /// <summary>The clock line of an SPI bus.</summary>
        SpiSck = 8,

        /// <summary>The line on which an SPI bus's controller sends.</summary>
        SpiMosi = 16,

        /// <summary>The line on which an SPI bus's controller receives.</summary>
        SpiMiso = 32,

        /// <summary>A chip select of an SPI bus.</summary>
        SpiCs = 64,

        /// <summary>The line on which a serial port sends.</summary>
        UartTx = 128,

        /// <summary>The line on which a serial port receives.</summary>
        UartRx = 256,

        /// <summary>The output of a PWM channel.</summary>
        Pwm = 512,

        /// <summary>An input of the chip's analog-to-digital converter.</summary>
        Adc = 1024,
    }
}
