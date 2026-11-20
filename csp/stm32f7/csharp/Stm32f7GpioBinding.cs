// The STM32F746's GPIO ports and external interrupt lines as Stm32GpioDriver takes them, every number from the part's generated layouts.
using Lamella.Generated;

internal sealed class Stm32f7GpioBinding
{
    private Stm32f7GpioBinding() { }

    /// <summary>The eleven ports, A to K: each one's base and clock gate, the port registers the driver
    /// reads and writes, their codes, and the mode a pin leaves reset in, input.</summary>
    internal static Stm32GpioPorts Ports()
    {
        uint clock = Stm32f7Instances.RCC_BASE;
        return new Stm32GpioPorts(
            new uint[]
            {
                Stm32f7Instances.GPIOA_BASE, Stm32f7Instances.GPIOB_BASE, Stm32f7Instances.GPIOC_BASE,
                Stm32f7Instances.GPIOD_BASE, Stm32f7Instances.GPIOE_BASE, Stm32f7Instances.GPIOF_BASE,
                Stm32f7Instances.GPIOG_BASE, Stm32f7Instances.GPIOH_BASE, Stm32f7Instances.GPIOI_BASE,
                Stm32f7Instances.GPIOJ_BASE, Stm32f7Instances.GPIOK_BASE,
            },
            new uint[]
            {
                clock + Stm32f7Instances.GPIOA_RCC_EN_OFF, clock + Stm32f7Instances.GPIOB_RCC_EN_OFF,
                clock + Stm32f7Instances.GPIOC_RCC_EN_OFF, clock + Stm32f7Instances.GPIOD_RCC_EN_OFF,
                clock + Stm32f7Instances.GPIOE_RCC_EN_OFF, clock + Stm32f7Instances.GPIOF_RCC_EN_OFF,
                clock + Stm32f7Instances.GPIOG_RCC_EN_OFF, clock + Stm32f7Instances.GPIOH_RCC_EN_OFF,
                clock + Stm32f7Instances.GPIOI_RCC_EN_OFF, clock + Stm32f7Instances.GPIOJ_RCC_EN_OFF,
                clock + Stm32f7Instances.GPIOK_RCC_EN_OFF,
            },
            new uint[]
            {
                Stm32f7Instances.GPIOA_RCC_EN_MASK, Stm32f7Instances.GPIOB_RCC_EN_MASK,
                Stm32f7Instances.GPIOC_RCC_EN_MASK, Stm32f7Instances.GPIOD_RCC_EN_MASK,
                Stm32f7Instances.GPIOE_RCC_EN_MASK, Stm32f7Instances.GPIOF_RCC_EN_MASK,
                Stm32f7Instances.GPIOG_RCC_EN_MASK, Stm32f7Instances.GPIOH_RCC_EN_MASK,
                Stm32f7Instances.GPIOI_RCC_EN_MASK, Stm32f7Instances.GPIOJ_RCC_EN_MASK,
                Stm32f7Instances.GPIOK_RCC_EN_MASK,
            },
            Stm32f7GpioLayout.MODER_OFF, Stm32f7GpioLayout.OTYPER_OFF, Stm32f7GpioLayout.PUPDR_OFF,
            Stm32f7GpioLayout.IDR_OFF, Stm32f7GpioLayout.BSRR_OFF,
            Stm32f7GpioLayout.MODER_MODE_INPUT, Stm32f7GpioLayout.MODER_MODE_OUTPUT,
            Stm32f7GpioLayout.MODER_MODE_ANALOG, Stm32f7GpioLayout.MODER_MODE_RESET,
            Stm32f7GpioLayout.PUPDR_NONE, Stm32f7GpioLayout.PUPDR_PULL_UP, Stm32f7GpioLayout.PUPDR_PULL_DOWN);
    }

    /// <summary>The external interrupt controller's GPIO lines: one pending register for either edge,
    /// and each line's port selected in SYSCFG, behind SYSCFG's clock gate.</summary>
    internal static Stm32ExtiLines Lines()
    {
        uint exti = Stm32f7Instances.EXTI_BASE;
        return new Stm32ExtiLines(
            exti + Stm32f7ExtiLayout.IMR_OFF, exti + Stm32f7ExtiLayout.RTSR_OFF, exti + Stm32f7ExtiLayout.FTSR_OFF,
            exti + Stm32f7ExtiLayout.PR_OFF, exti + Stm32f7ExtiLayout.PR_OFF,
            Stm32f7Instances.SYSCFG_BASE + Stm32f7SyscfgLayout.EXTICR1_OFF,
            (int)(Stm32f7SyscfgLayout.EXTICR1_EXTI1_LSB - Stm32f7SyscfgLayout.EXTICR1_EXTI0_LSB),
            Stm32f7Instances.RCC_BASE + Stm32f7Instances.SYSCFG_RCC_EN_OFF, Stm32f7Instances.SYSCFG_RCC_EN_MASK);
    }
}
