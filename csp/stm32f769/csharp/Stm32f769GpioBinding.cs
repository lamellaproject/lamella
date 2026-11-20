// The STM32F769's GPIO ports and external interrupt lines as Stm32GpioDriver takes them, every number from the part's generated layouts.
using Lamella.Generated;

internal sealed class Stm32f769GpioBinding
{
    private Stm32f769GpioBinding() { }

    /// <summary>The eleven ports, A to K: each one's base and clock gate, the port registers the driver
    /// reads and writes, their codes, and the mode a pin leaves reset in, input.</summary>
    internal static Stm32GpioPorts Ports()
    {
        uint clock = Stm32f769Instances.RCC_BASE;
        return new Stm32GpioPorts(
            new uint[]
            {
                Stm32f769Instances.GPIOA_BASE, Stm32f769Instances.GPIOB_BASE, Stm32f769Instances.GPIOC_BASE,
                Stm32f769Instances.GPIOD_BASE, Stm32f769Instances.GPIOE_BASE, Stm32f769Instances.GPIOF_BASE,
                Stm32f769Instances.GPIOG_BASE, Stm32f769Instances.GPIOH_BASE, Stm32f769Instances.GPIOI_BASE,
                Stm32f769Instances.GPIOJ_BASE, Stm32f769Instances.GPIOK_BASE,
            },
            new uint[]
            {
                clock + Stm32f769Instances.GPIOA_RCC_EN_OFF, clock + Stm32f769Instances.GPIOB_RCC_EN_OFF,
                clock + Stm32f769Instances.GPIOC_RCC_EN_OFF, clock + Stm32f769Instances.GPIOD_RCC_EN_OFF,
                clock + Stm32f769Instances.GPIOE_RCC_EN_OFF, clock + Stm32f769Instances.GPIOF_RCC_EN_OFF,
                clock + Stm32f769Instances.GPIOG_RCC_EN_OFF, clock + Stm32f769Instances.GPIOH_RCC_EN_OFF,
                clock + Stm32f769Instances.GPIOI_RCC_EN_OFF, clock + Stm32f769Instances.GPIOJ_RCC_EN_OFF,
                clock + Stm32f769Instances.GPIOK_RCC_EN_OFF,
            },
            new uint[]
            {
                Stm32f769Instances.GPIOA_RCC_EN_MASK, Stm32f769Instances.GPIOB_RCC_EN_MASK,
                Stm32f769Instances.GPIOC_RCC_EN_MASK, Stm32f769Instances.GPIOD_RCC_EN_MASK,
                Stm32f769Instances.GPIOE_RCC_EN_MASK, Stm32f769Instances.GPIOF_RCC_EN_MASK,
                Stm32f769Instances.GPIOG_RCC_EN_MASK, Stm32f769Instances.GPIOH_RCC_EN_MASK,
                Stm32f769Instances.GPIOI_RCC_EN_MASK, Stm32f769Instances.GPIOJ_RCC_EN_MASK,
                Stm32f769Instances.GPIOK_RCC_EN_MASK,
            },
            Stm32f769GpioLayout.MODER_OFF, Stm32f769GpioLayout.OTYPER_OFF, Stm32f769GpioLayout.PUPDR_OFF,
            Stm32f769GpioLayout.IDR_OFF, Stm32f769GpioLayout.BSRR_OFF,
            Stm32f769GpioLayout.MODER_MODE_INPUT, Stm32f769GpioLayout.MODER_MODE_OUTPUT,
            Stm32f769GpioLayout.MODER_MODE_ANALOG, Stm32f769GpioLayout.MODER_MODE_RESET,
            Stm32f769GpioLayout.PUPDR_NONE, Stm32f769GpioLayout.PUPDR_PULL_UP, Stm32f769GpioLayout.PUPDR_PULL_DOWN);
    }

    /// <summary>The external interrupt controller's GPIO lines: one pending register for either edge,
    /// and each line's port selected in SYSCFG, behind SYSCFG's clock gate.</summary>
    internal static Stm32ExtiLines Lines()
    {
        uint exti = Stm32f769Instances.EXTI_BASE;
        return new Stm32ExtiLines(
            exti + Stm32f769ExtiLayout.IMR_OFF, exti + Stm32f769ExtiLayout.RTSR_OFF, exti + Stm32f769ExtiLayout.FTSR_OFF,
            exti + Stm32f769ExtiLayout.PR_OFF, exti + Stm32f769ExtiLayout.PR_OFF,
            Stm32f769Instances.SYSCFG_BASE + Stm32f769SyscfgLayout.EXTICR1_OFF,
            (int)(Stm32f769SyscfgLayout.EXTICR1_EXTI1_LSB - Stm32f769SyscfgLayout.EXTICR1_EXTI0_LSB),
            Stm32f769Instances.RCC_BASE + Stm32f769Instances.SYSCFG_RCC_EN_OFF, Stm32f769Instances.SYSCFG_RCC_EN_MASK);
    }
}
