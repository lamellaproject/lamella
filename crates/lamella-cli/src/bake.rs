//! Baking a compiled assembly into the flash image a device runs.

use lamella_wire_host::engine::LcscCompiler;

/// Bake a compiled assembly into a `.lmli` flash image.
///
/// Single-assembly: BCL references resolve to the interpreter's intrinsics, which is the same
/// shape the baked-image link deploys over the wire.
///
/// # Errors
/// When the emitted assembly does not parse, does not load, or cannot be laid out for flash.
pub fn bake(assembly: Vec<u8>) -> Result<Vec<u8>, String> {
    let program: &'static [u8] = Box::leak(assembly.into_boxed_slice());
    let parsed = lamella_metadata::Assembly::read(program)
        .map_err(|error| format!("the emitted assembly does not parse: {error:?}"))?;
    let loaded = lamella_load::load(&parsed).map_err(|error| format!("load: {error}"))?;
    let mut module = loaded.module;
    module
        .write_baked(Some(loaded.entry))
        .map_err(|error| format!("bake: {error:?}"))
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
