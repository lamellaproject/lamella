//! Baking a compiled assembly into the flash image a device runs.

use lamella_wire_host::engine::LcscCompiler;

/// Bake a compiled assembly into a `.lmli` flash image.
///
/// Single-assembly: BCL references resolve to the interpreter's intrinsics, which is the same
/// shape the baked-image link deploys over the wire.
///
/// The program is checked against the capability profile this build carries BEFORE an image is
/// written, so a body this tier cannot execute is refused here rather than on the board.
///
/// # Errors
/// When the emitted assembly does not parse, does not load, exceeds the capability profile, or
/// cannot be laid out for flash.
pub fn bake(assembly: Vec<u8>) -> Result<Vec<u8>, String> {
    let program: &'static [u8] = Box::leak(assembly.into_boxed_slice());
    let parsed = lamella_metadata::Assembly::read(program)
        .map_err(|error| format!("the emitted assembly does not parse: {error:?}"))?;
    let loaded = lamella_load::load(&parsed).map_err(|error| format!("load: {error}"))?;
    let mut module = loaded.module;
    let violations = module.validate_profile(None);
    if !violations.is_empty() {
        return Err(profile_refusal(&violations));
    }
    module
        .write_baked(Some(loaded.entry))
        .map_err(|error| format!("bake: {error:?}"))
}

/// The refusal text for a program whose baked image would not run on the device.
///
/// Every violation names the method it is in, so the list IS the diagnosis. It is capped because a
/// program that misses the tier badly misses it in hundreds of places, and a wall of them buries
/// the first one -- which is the one worth reading.
fn profile_refusal(violations: &[impl core::fmt::Display]) -> String {
    const SHOWN: usize = 20;
    let places = if violations.len() == 1 {
        String::from("one place")
    } else {
        format!("{} places", violations.len())
    };
    let mut text =
        format!("bake: no image written. This program would trap on the device in {places}:");
    for violation in violations.iter().take(SHOWN) {
        text.push_str(&format!("\n  {violation}"));
    }
    if violations.len() > SHOWN {
        text.push_str(&format!("\n  ... and {} more", violations.len() - SHOWN));
    }
    text
}

/// Compile `source` and bake it in one step -- what `deploy` needs and what `build` does under
/// this feature.
///
/// # Errors
/// A compile failure, reported as the compiler wrote it, or any bake failure.
pub fn compile_and_bake(compiler: &LcscCompiler, source: &str) -> Result<Vec<u8>, String> {
    use lamella_wire_host::engine::{CompileFailure, ReplCompiler};
    let assembly = compiler.compile(source).map_err(|failure| match failure {
        CompileFailure::Diagnostics(text) => text,
        CompileFailure::Toolchain(text) => format!("toolchain error: {text}"),
    })?;
    bake(assembly)
}
