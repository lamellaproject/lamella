//! Per-chip peripheral register facts, one leaf file per chip/peripheral pair, each transcribed
//! from its neutral peripheral-table SSOT -- the table is the source of truth,
//! these modules its in-runtime binding. Keeping them in dedicated leaves keeps the core
//! peripheral machinery (`gpio`, `uart`) chip-neutral: a new chip is a new file here plus its
//! dispatch arm, never an edit inside the core.

pub(crate) mod gpio_rp2350;
pub(crate) mod gpio_stm32f4;
pub(crate) mod uart_esp32c6;
pub(crate) mod uart_rp2040;
pub(crate) mod uart_rp2350;
pub(crate) mod uart_samd21;
pub(crate) mod spi_rp2350;
pub(crate) mod i2c_rp2350;
pub(crate) mod adc_rp2350;
