// The registers of an STM32 family's external interrupt controller that Stm32GpioDriver arms a pin's line with.
internal sealed class Stm32ExtiLines
{
    /// <summary>The address of the interrupt mask register, a bit per line, 1 unmasked.</summary>
    internal readonly uint Mask;

    /// <summary>The addresses of the rising and falling trigger enable registers, a bit per line.</summary>
    internal readonly uint RisingTrigger;
    internal readonly uint FallingTrigger;

    /// <summary>The addresses of the registers a rising and a falling edge are pending in, a bit per line,
    /// each write-one-to-clear. One register on a controller that does not tell the edges apart.</summary>
    internal readonly uint RisingPending;
    internal readonly uint FallingPending;

    /// <summary>The address of the first register of port selectors, line 0's selector in its lowest
    /// field; each register holds four lines' selectors.</summary>
    internal readonly uint Selectors;

    /// <summary>How many bits one line's port selector takes.</summary>
    internal readonly int SelectorBits;

    /// <summary>The address of the RCC register gating the peripheral the selectors are in, and the
    /// gate's bit; both 0 where nothing gates it.</summary>
    internal readonly uint SelectorClockRegister;
    internal readonly uint SelectorClockMask;

    internal Stm32ExtiLines(uint mask, uint risingTrigger, uint fallingTrigger, uint risingPending, uint fallingPending,
        uint selectors, int selectorBits, uint selectorClockRegister, uint selectorClockMask)
    {
        Mask = mask;
        RisingTrigger = risingTrigger;
        FallingTrigger = fallingTrigger;
        RisingPending = risingPending;
        FallingPending = fallingPending;
        Selectors = selectors;
        SelectorBits = selectorBits;
        SelectorClockRegister = selectorClockRegister;
        SelectorClockMask = selectorClockMask;
    }
}
