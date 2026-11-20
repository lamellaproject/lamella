// The STM32U5A5's GPIO ports and external interrupt lines as Stm32GpioDriver takes them, every number from the part's generated layouts.
using Lamella.Generated;

internal sealed class Stm32u5a5GpioBinding
{
    private Stm32u5a5GpioBinding() { }

    /// <summary>The ten ports, A to J: each one's base and clock gate, the port registers the driver
    /// reads and writes, their codes, and the mode a pin leaves reset in, analog.</summary>
    internal static Stm32GpioPorts Ports()
    {
        uint clock = Stm32u5a5Instances.RCC_BASE;
        return new Stm32GpioPorts(
            new uint[]
            {
                Stm32u5a5Instances.GPIOA_BASE, Stm32u5a5Instances.GPIOB_BASE, Stm32u5a5Instances.GPIOC_BASE,
                Stm32u5a5Instances.GPIOD_BASE, Stm32u5a5Instances.GPIOE_BASE, Stm32u5a5Instances.GPIOF_BASE,
                Stm32u5a5Instances.GPIOG_BASE, Stm32u5a5Instances.GPIOH_BASE, Stm32u5a5Instances.GPIOI_BASE,
                Stm32u5a5Instances.GPIOJ_BASE,
            },
            new uint[]
            {
                clock + Stm32u5a5Instances.GPIOA_RCC_EN_OFF, clock + Stm32u5a5Instances.GPIOB_RCC_EN_OFF,
                clock + Stm32u5a5Instances.GPIOC_RCC_EN_OFF, clock + Stm32u5a5Instances.GPIOD_RCC_EN_OFF,
                clock + Stm32u5a5Instances.GPIOE_RCC_EN_OFF, clock + Stm32u5a5Instances.GPIOF_RCC_EN_OFF,
                clock + Stm32u5a5Instances.GPIOG_RCC_EN_OFF, clock + Stm32u5a5Instances.GPIOH_RCC_EN_OFF,
                clock + Stm32u5a5Instances.GPIOI_RCC_EN_OFF, clock + Stm32u5a5Instances.GPIOJ_RCC_EN_OFF,
            },
            new uint[]
            {
                Stm32u5a5Instances.GPIOA_RCC_EN_MASK, Stm32u5a5Instances.GPIOB_RCC_EN_MASK,
                Stm32u5a5Instances.GPIOC_RCC_EN_MASK, Stm32u5a5Instances.GPIOD_RCC_EN_MASK,
                Stm32u5a5Instances.GPIOE_RCC_EN_MASK, Stm32u5a5Instances.GPIOF_RCC_EN_MASK,
                Stm32u5a5Instances.GPIOG_RCC_EN_MASK, Stm32u5a5Instances.GPIOH_RCC_EN_MASK,
                Stm32u5a5Instances.GPIOI_RCC_EN_MASK, Stm32u5a5Instances.GPIOJ_RCC_EN_MASK,
            },
            Stm32u5a5GpioLayout.MODER_OFF, Stm32u5a5GpioLayout.OTYPER_OFF, Stm32u5a5GpioLayout.PUPDR_OFF,
            Stm32u5a5GpioLayout.IDR_OFF, Stm32u5a5GpioLayout.BSRR_OFF,
            Stm32u5a5GpioLayout.MODER_MODE_INPUT, Stm32u5a5GpioLayout.MODER_MODE_OUTPUT,
            Stm32u5a5GpioLayout.MODER_MODE_ANALOG, Stm32u5a5GpioLayout.MODER_MODE_RESET,
            Stm32u5a5GpioLayout.PUPDR_NONE, Stm32u5a5GpioLayout.PUPDR_PULL_UP, Stm32u5a5GpioLayout.PUPDR_PULL_DOWN);
    }

    /// <summary>The external interrupt controller's GPIO lines: a pending register for each edge, and
    /// each line's port selected in EXTI itself, which no RCC register gates.</summary>
    internal static Stm32ExtiLines Lines()
    {
        uint exti = Stm32u5a5Instances.EXTI_BASE;
        return new Stm32ExtiLines(
            exti + Stm32u5a5ExtiLayout.IMR1_OFF, exti + Stm32u5a5ExtiLayout.RTSR1_OFF, exti + Stm32u5a5ExtiLayout.FTSR1_OFF,
            exti + Stm32u5a5ExtiLayout.RPR1_OFF, exti + Stm32u5a5ExtiLayout.FPR1_OFF,
            exti + Stm32u5a5ExtiLayout.EXTICR1_OFF,
            (int)(Stm32u5a5ExtiLayout.EXTICR1_EXTI1_LSB - Stm32u5a5ExtiLayout.EXTICR1_EXTI0_LSB),
            0u, 0u);
    }
}
