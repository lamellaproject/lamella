//! The volatile memory-mapped-I/O primitive: 8/16/32-bit `write_volatile`/`read_volatile` at a raw
//! address. This is the ONE place the interpreter's `Lamella.Hardware.Mmio` seam reaches real device
//! registers. It lives in its own crate BECAUSE it is inherently `unsafe` (a raw register poke), so
//! the no_std interpreter core -- which `forbid(unsafe_code)` -- installs these SAFE fn pointers
//! without taking on unsafe itself. On a host an arbitrary register address is not real memory, so a
//! device firmware / the runner installs these only on `target_os = "none"`; the host interpreter
//! uses its simulated register file instead. On AOT, `Mmio` lowers to an inline volatile `str`/`ldr`
//! and this crate is not involved.
#![no_std]
#![allow(unsafe_code)]

/// A volatile 32-bit store of `value` to the register at `address`.
pub fn write32(address: u32, value: u32) {
    unsafe { core::ptr::write_volatile(address as *mut u32, value) };
}

/// A volatile 32-bit load from the register at `address`.
#[must_use]
pub fn read32(address: u32) -> u32 {
    unsafe { core::ptr::read_volatile(address as *const u32) }
}

/// A volatile 8-bit store of `value` to the byte register at `address`.
pub fn write8(address: u32, value: u8) {
    unsafe { core::ptr::write_volatile(address as *mut u8, value) };
}

/// A volatile 8-bit load from the byte register at `address`.
#[must_use]
pub fn read8(address: u32) -> u8 {
    unsafe { core::ptr::read_volatile(address as *const u8) }
}

/// A volatile 16-bit store of `value` to the halfword register at `address` (2-byte aligned).
pub fn write16(address: u32, value: u16) {
    unsafe { core::ptr::write_volatile(address as *mut u16, value) };
}

/// A volatile 16-bit load from the halfword register at `address` (2-byte aligned).
#[must_use]
pub fn read16(address: u32) -> u16 {
    unsafe { core::ptr::read_volatile(address as *const u16) }
}
