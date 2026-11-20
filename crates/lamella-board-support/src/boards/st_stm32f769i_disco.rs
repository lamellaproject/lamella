//! The STM32F769I-DISCO: an STM32F769NI, whose millisecond clock counts on TIM5.

use crate::stm32;

#[allow(dead_code)]
#[path = "../../../../csp/stm32f769/rust/stm32f769_instances.rs"]
mod instances;
#[allow(dead_code)]
#[path = "../../../../csp/stm32f769/rust/stm32f769_rcc_layout.rs"]
mod rcc;
#[allow(dead_code)]
#[path = "../../../../csp/stm32f769/rust/stm32f769_tim_layout.rs"]
mod tim;

/// The board's millisecond clock as the part's generated facts state it: TIM5, one of its two 32-bit
/// timers, counting the clock an APB1 timer runs at from reset.
pub const CLOCK: stm32::Timer = stm32::Timer {
    clock_register: instances::RCC_BASE + instances::TIM5_RCC_EN_OFF,
    clock_mask: instances::TIM5_RCC_EN_MASK,
    control: instances::TIM5_BASE + tim::CR1_OFF,
    counter_enable: tim::CR1_CEN,
    event_generation: instances::TIM5_BASE + tim::EGR_OFF,
    update_generation: tim::EGR_UG,
    prescaler: instances::TIM5_BASE + tim::PSC_OFF,
    prescaler_mask: tim::PSC_PSC,
    counter: instances::TIM5_BASE + tim::CNT_OFF,
    auto_reload_reset: tim::ARR_RESET,
    clock_hz: rcc::APB1_TIMER_RESET_HZ,
}
.checked();
