//! Hands the bare-metal link its memory layout. Host builds (the stub main) link normally.

fn main() {
    println!("cargo:rerun-if-changed=memory-stm32f7.x");
    println!("cargo:rerun-if-changed=memory-stm32f769.x");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("none") {
        // One map per part, and the default is the STM32F746's. Both scripts carry the core
        // exception vectors and USART1's slot, the one external interrupt the firmware enables,
        // and each states its part's deploy window: they differ in where memory is, how much of it
        // there is, and where in flash the window sits.
        //
        // A build that names its part takes that part's own flash and SRAM, so `stm32f769` wins
        // over the default whenever it is asked for. The firmware takes exactly one board feature:
        // it refuses `st-stm32f746g-disco` together with `stm32f769` (which `st-stm32f769i-disco`
        // turns on), so `--all-features` builds no image. An image linked at the F769's layout must
        // never reach an F746: its initial stack pointer lies 192 KB past the end of that part's
        // SRAM.
        let layout = if std::env::var("CARGO_FEATURE_STM32F769").is_ok() {
            "memory-stm32f769.x"
        } else {
            "memory-stm32f7.x"
        };
        let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(layout);
        println!("cargo:rustc-link-arg-bins=-T{}", script.display());
    }
}
