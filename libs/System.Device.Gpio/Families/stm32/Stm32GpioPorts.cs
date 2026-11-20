// The registers of an STM32 family's GPIO ports that Stm32GpioDriver drives, as the family's generated layouts place them.
internal sealed class Stm32GpioPorts
{
    /// <summary>Each port's register base, port A first.</summary>
    internal readonly uint[] Bases;

    /// <summary>The address of the RCC register holding each port's clock enable, port A first.</summary>
    internal readonly uint[] ClockRegisters;

    /// <summary>Each port's clock enable bit in its <see cref="ClockRegisters"/> entry, port A first.</summary>
    internal readonly uint[] ClockMasks;

    /// <summary>The offsets of MODER, OTYPER, PUPDR, IDR and BSRR from a port's base.</summary>
    internal readonly uint Moder;
    internal readonly uint Otyper;
    internal readonly uint Pupdr;
    internal readonly uint Idr;
    internal readonly uint Bsrr;

    /// <summary>MODER's input and output codes, and the mode a pin leaves reset in, which a closed pin
    /// returns to.</summary>
    internal readonly uint ModeInput;
    internal readonly uint ModeOutput;
    internal readonly uint ModeAnalog;
    internal readonly uint ModeReset;

    /// <summary>PUPDR's codes for no pull, a pull-up and a pull-down.</summary>
    internal readonly uint PullNone;
    internal readonly uint PullUp;
    internal readonly uint PullDown;

    /// <exception cref="System.ArgumentException">The three per-port arrays differ in length.</exception>
    internal Stm32GpioPorts(uint[] bases, uint[] clockRegisters, uint[] clockMasks,
        uint moder, uint otyper, uint pupdr, uint idr, uint bsrr,
        uint modeInput, uint modeOutput, uint modeAnalog, uint modeReset,
        uint pullNone, uint pullUp, uint pullDown)
    {
        if (clockRegisters.Length != bases.Length || clockMasks.Length != bases.Length)
        {
            throw new System.ArgumentException("each port needs exactly one base, one clock register and one clock mask");
        }
        Bases = bases;
        ClockRegisters = clockRegisters;
        ClockMasks = clockMasks;
        Moder = moder;
        Otyper = otyper;
        Pupdr = pupdr;
        Idr = idr;
        Bsrr = bsrr;
        ModeInput = modeInput;
        ModeOutput = modeOutput;
        ModeAnalog = modeAnalog;
        ModeReset = modeReset;
        PullNone = pullNone;
        PullUp = pullUp;
        PullDown = pullDown;
    }
}
