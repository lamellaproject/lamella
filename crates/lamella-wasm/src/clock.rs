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
pub fn install(vm: &mut lamella_cil_runtime::Vm) {
    vm.set_clock(now_millis, sleep_millis);

    let unix_millis = wall_unix_millis();
    if unix_millis > 0 {
        const UNIX_EPOCH_IN_NET_TICKS: i64 = 621_355_968_000_000_000;
        let ticks = i64::try_from(unix_millis)
            .ok()
            .and_then(|ms| ms.checked_mul(10_000))
            .and_then(|t| t.checked_add(UNIX_EPOCH_IN_NET_TICKS));
        if let Some(ticks) = ticks {
            vm.set_now_ticks(ticks);
        }
    }
}
