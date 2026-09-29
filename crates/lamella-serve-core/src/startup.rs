//! The shared bare-metal startup for the firmware binaries, each of which
//! `#[path]`-includes it from this crate.
//! The vector table (memory*.x) points Reset at `reset`, which establishes the Rust
//! runtime -- zero `.bss`, copy `.data` from its flash load address -- before handing off
//! to the binary's `lamella_main`. Each binary also exports `fault`, the handler every
//! populated exception vector names.

use core::ptr::{read_volatile, write_volatile};

unsafe extern "C" {
    static mut _sbss: u32;
    static mut _ebss: u32;
    static mut _sdata: u32;
    static mut _edata: u32;
    static _sidata: u32;
    fn lamella_main() -> !;
}

#[unsafe(no_mangle)]
pub extern "C" fn reset() -> ! {
    unsafe {
        #[cfg(target_abi = "eabihf")]
        {
            const CPACR: *mut u32 = 0xe000_ed88 as *mut u32;
            write_volatile(CPACR, read_volatile(CPACR) | (0b11 << 20) | (0b11 << 22));
            core::arch::asm!("dsb", "isb");
        }

        let mut word = &raw mut _sbss;
        let end = &raw mut _ebss;
        while word < end {
            write_volatile(word, 0);
            word = word.add(1);
        }

        let mut destination = &raw mut _sdata;
        let end = &raw mut _edata;
        let mut source = &raw const _sidata;
        while destination < end {
            write_volatile(destination, read_volatile(source));
            destination = destination.add(1);
            source = source.add(1);
        }

        lamella_main()
    }
}
