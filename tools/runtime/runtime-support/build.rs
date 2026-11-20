//! Sizes the console ring (`console-memory`) for the part the archive's target serves. The archive is
//! built once per instruction set. On a Nordic part the linked image lays the ring among its statics,
//! and the heap takes the RAM they leave, so every byte of the ring is one the heap does not have; on
//! the RP2350 the statics window is most of RAM. So the size follows the target, and a Thumb-1 board
//! build whose RAM has the room asks for the larger ring with `console-ring-16k`.

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rustc-check-cfg=cfg(console_ring_16k)");
    let target = std::env::var("TARGET").unwrap_or_default();
    let asked = std::env::var_os("CARGO_FEATURE_CONSOLE_RING_16K").is_some();
    if target.starts_with("thumbv8m") || asked {
        println!("cargo::rustc-cfg=console_ring_16k");
    }
}
