//! The MicroPython / CircuitPython compatibility shims' dispatch tables: flavor ids, the
//! flavor-gated method unions, and the shim instance layouts. A shim is a thin face over the
//! Lamella-standard surface (`gpio`, `uart`); everything here is face-plumbing, kept outside
//! both the standard modules and the chip tables.

pub(crate) mod uart;
pub(crate) mod spi;
pub(crate) mod i2c;
pub(crate) mod adc;
