//! Hands the bare-metal link its memory layout. Host builds (the stub main) link normally.

fn main() {
    println!("cargo:rerun-if-changed=memory-stm32u5a5.x");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("none") {
        // One part in this family so far, so one map: the STM32U5A5's. A second part takes its own
        // file, chosen here by its feature, because a map states where memory is and how much of it
        // there is, and both are part facts.
        let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("memory-stm32u5a5.x");
        println!("cargo:rustc-link-arg-bins=-T{}", script.display());
    }
}
