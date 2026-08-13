//! The host STDOUT seam: console output streamed as the program produces it, rather than handed over in
//! one buffer when the program ends.

/// Called for every console write, with the UTF-16 units of that write.
#[cfg(target_arch = "wasm32")]
mod imported {
    #[link(wasm_import_module = "lamella_host")]
    unsafe extern "C" {
        pub safe fn write_stdout(ptr: *const u16, len: usize);
    }
}

#[cfg(target_arch = "wasm32")]
fn tap(chars: &[u16]) {
    imported::write_stdout(chars.as_ptr(), chars.len());
}

#[cfg(not(target_arch = "wasm32"))]
thread_local! {
    static STREAMED: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(not(target_arch = "wasm32"))]
fn tap(chars: &[u16]) {
    let text = String::from_utf16_lossy(chars);
    STREAMED.with(|s| s.borrow_mut().push(text));
}

/// Takes the chunks the native tap has collected since the last call, in the order they were written.
/// Test-only: the wasm host reads its stream through the import instead.
#[cfg(not(target_arch = "wasm32"))]
#[must_use]
pub fn take_streamed() -> Vec<String> {
    STREAMED.with(|s| core::mem::take(&mut *s.borrow_mut()))
}

/// Installs the streaming stdout tap on `vm`. Call this on every `Vm` this crate creates: a `Vm` without it
/// still produces correct output, but the host sees none of it until the program ends.
pub fn install(vm: &mut lamella_cil_runtime::Vm) {
    vm.set_console_tap(tap);
}
