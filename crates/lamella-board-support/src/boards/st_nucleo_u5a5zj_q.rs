//! The NUCLEO-U5A5ZJ-Q: an STM32U5A5ZJT6Q whose VDDIO2 rail powers PG2 to PG15, among them the pad of
//! its red user LED, LD3, on PG2, and whose millisecond clock counts on TIM5.

use crate::{stm32, stm32u5};

#[allow(dead_code)]
#[path = "../../../../bsp/boards/st-nucleo-u5a5zj-q/rust/st_nucleo_u5a5zj_q_bindings.rs"]
mod bindings;
#[allow(dead_code)]
#[path = "../../../../csp/stm32u5a5/rust/stm32u5a5_instances.rs"]
mod instances;
#[allow(dead_code)]
#[path = "../../../../csp/stm32u5a5/rust/stm32u5a5_rcc_layout.rs"]
mod rcc;
#[allow(dead_code)]
#[path = "../../../../csp/stm32u5a5/rust/stm32u5a5_tim_layout.rs"]
mod tim;

/// The VDDIO2 rail as the board's generated bindings state it: PWR's clock gate, and PWR_SVMCR's
/// IO2SV bit, which declares the rail valid.
pub const VDDIO2: stm32u5::Rail = stm32u5::Rail {
    clock_register: bindings::SUPPLY_VDDIO2_RCC_EN_REG,
    clock_mask: bindings::SUPPLY_VDDIO2_RCC_EN_MASK,
    valid_register: bindings::SUPPLY_VDDIO2_VALID_REG,
    valid_mask: bindings::SUPPLY_VDDIO2_VALID_MASK,
};

/// The board's millisecond clock as the part's generated facts state it: TIM5, at its nonsecure
/// alias, counting the clock an APB1 timer runs at from reset.
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
