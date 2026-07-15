//! The MicroPython `machine.UART` / CircuitPython `busio.UART` shim tables (thin faces over
//! the clean uart API, exactly as `machine.Pin` / `digitalio` are over `gpio`).

/// Which shimmed API a shim factory/instance wears -- the flavor gates the visible names
/// and holds each shimmed API's quirks (constructor-held timeouts, None-on-timeout returns,
/// numeric parity codes, float-second timeouts) OUTSIDE the standard surface.
pub(crate) const SHIM_FLAVOR_MACHINE: u32 = 0;
pub(crate) const SHIM_FLAVOR_BUSIO: u32 = 1;

/// Method ids for a shim UART instance (dispatched in `ObjectModel::call_uart_shim_method`).
pub(crate) const SHIM_READ: u32 = 0;
pub(crate) const SHIM_READINTO: u32 = 1;
pub(crate) const SHIM_READLINE: u32 = 2;
pub(crate) const SHIM_WRITE: u32 = 3;
pub(crate) const SHIM_ANY: u32 = 4;
pub(crate) const SHIM_DEINIT: u32 = 5;
pub(crate) const SHIM_FLUSH: u32 = 6;
pub(crate) const SHIM_RESET_INPUT: u32 = 7;
pub(crate) const SHIM_ENTER: u32 = 8;
pub(crate) const SHIM_EXIT: u32 = 9;

/// The shim method id for `name` under `flavor` (the union surface, flavor-gated where the
/// shimmed APIs differ: `any`/`flush` are MicroPython's, `reset_input_buffer` CircuitPython's).
pub(crate) fn uart_shim_method_id(flavor: u32, name: &str) -> Option<u32> {
    match name {
        "read" => Some(SHIM_READ),
        "readinto" => Some(SHIM_READINTO),
        "readline" => Some(SHIM_READLINE),
        "write" => Some(SHIM_WRITE),
        "deinit" => Some(SHIM_DEINIT),
        "any" if flavor == SHIM_FLAVOR_MACHINE => Some(SHIM_ANY),
        "flush" if flavor == SHIM_FLAVOR_MACHINE => Some(SHIM_FLUSH),
        "reset_input_buffer" if flavor == SHIM_FLAVOR_BUSIO => Some(SHIM_RESET_INPUT),
        "__enter__" if flavor == SHIM_FLAVOR_BUSIO => Some(SHIM_ENTER),
        "__exit__" if flavor == SHIM_FLAVOR_BUSIO => Some(SHIM_EXIT),
        _ => None,
    }
}

pub(crate) const SHIM_W_PORT: u32 = 0;
pub(crate) const SHIM_W_TIMEOUT_MS: u32 = 1;
pub(crate) const SHIM_W_FLAVOR: u32 = 2;
pub(crate) const SHIM_WORDS: u32 = 3;
