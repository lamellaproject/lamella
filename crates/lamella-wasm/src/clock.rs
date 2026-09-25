//! The host clock seam for a WebAssembly embedding.

#[cfg(target_arch = "wasm32")]
mod imported {
    #[link(wasm_import_module = "lamella_host")]
    unsafe extern "C" {
        pub safe fn now_millis() -> u64;
        pub safe fn sleep_millis(millis: u64);
        pub safe fn wall_unix_millis() -> u64;
    }
}

/// Monotonic milliseconds from the host.
#[cfg(target_arch = "wasm32")]
fn now_millis() -> u64 {
    imported::now_millis()
}

/// Block the calling thread for `millis`, if this host can block.
#[cfg(target_arch = "wasm32")]
fn sleep_millis(millis: u64) {
    imported::sleep_millis(millis);
}

/// Milliseconds since the Unix epoch, or 0 when this host does not know the wall time.
#[cfg(target_arch = "wasm32")]
fn wall_unix_millis() -> u64 {
    imported::wall_unix_millis()
}

#[cfg(not(target_arch = "wasm32"))]
fn now_millis() -> u64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static BASE: OnceLock<Instant> = OnceLock::new();
    u64::try_from(BASE.get_or_init(Instant::now).elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(not(target_arch = "wasm32"))]
fn sleep_millis(millis: u64) {
    std::thread::sleep(std::time::Duration::from_millis(millis));
}

#[cfg(not(target_arch = "wasm32"))]
fn wall_unix_millis() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(0))
}

/// Installs the host clock seam on `vm`. Call this on every `Vm` this crate creates -- a `Vm` without it
/// reports a frozen clock and a `Thread.Sleep` that does not sleep, with no diagnostic.
pub fn install(module: &lamella_cil_runtime::Module, vm: &mut lamella_cil_runtime::Vm) {
    vm.set_clock(now_millis, sleep_millis);

    if let Some(ticks) = wall_ticks() {
        lamella_cil_runtime::set_wall_clock(module, vm, ticks);
    }
}

/// Installs the same clock as [`install`] through a runner's configure hook, which hands its
/// embedder the machine and not the module: the hook the REPL's loopback link calls for every
/// submission.
///
/// The wall clock goes in through [`lamella_cil_runtime::Vm::set_now_ticks`] rather than the managed
/// setter [`install`] calls. The hook runs before the program's module is loaded, and the runner
/// publishes that anchor into the managed clock once it is.
pub fn configure(vm: &mut lamella_cil_runtime::Vm) {
    vm.set_clock(now_millis, sleep_millis);
    if let Some(ticks) = wall_ticks() {
        vm.set_now_ticks(ticks);
    }
}

/// The host's wall clock in .NET ticks (100 ns since 0001-01-01), or `None` when the host does not
/// know the time.
fn wall_ticks() -> Option<i64> {
    const UNIX_EPOCH_IN_NET_TICKS: i64 = 621_355_968_000_000_000;
    let unix_millis = wall_unix_millis();
    if unix_millis == 0 {
        return None;
    }
    i64::try_from(unix_millis)
        .ok()
        .and_then(|ms| ms.checked_mul(10_000))
        .and_then(|t| t.checked_add(UNIX_EPOCH_IN_NET_TICKS))
}
