//! Runtime-support object for the AOT linked image, cross-compiled for thumbv6m and linked in on demand
//! (lamella-link's archive path pulls only the members a program reaches). It supplies the two runtime
//! symbol groups the backend emits but does not define itself: `lamella_gc_alloc`, the device GC-alloc
//! entry, and the `__aeabi_*` soft-float helpers a `double`/`float` op lowers to.
#![no_std]
#![allow(
    clippy::not_unsafe_ptr_arg_deref,
    clippy::while_immutable_condition,
    clippy::empty_loop,
    clippy::neg_cmp_op_on_partial_ord
)]

use core::panic::PanicInfo;

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    loop {}
}

#[cfg(feature = "net")]
pub use lamella_runtime_support_net as net_support;

#[cfg(feature = "net")]
mod net_alloc {
    struct NetHeap;

    unsafe impl core::alloc::GlobalAlloc for NetHeap {
        unsafe fn alloc(&self, layout: core::alloc::Layout) -> *mut u8 {
            crate::net_support::rt_alloc(layout)
        }

        unsafe fn dealloc(&self, _pointer: *mut u8, _layout: core::alloc::Layout) {}
    }

    #[global_allocator]
    static ALLOCATOR: NetHeap = NetHeap;
}

/// A RAM word holding the bump allocator's high-water pointer (the reset stub seeds it before the entry).
const HEAP_PTR: *mut u32 = 0x2000_0100 as *mut u32;

/// The word beside the cursor holding the heap's END -- one past the last byte the allocator may
/// hand out. The reset stub seeds it alongside [`HEAP_PTR`].
///
/// It cannot be a constant in this archive. A board's RAM extent is a board FACT -- how much SRAM,
/// which bank, what else already lives in it -- and this archive is built once per ISA for EVERY
/// board, exactly like the console sink. So the image supplies the number and the archive enforces
/// it, which is the same split the `console_sink` knob makes for the UART.
///
/// **A zero end is not "unbounded", it is "no heap": every allocation fails.** That direction is
/// deliberate. The opposite reading -- treat 0 as no limit -- would make an un-seeded stub silently
/// keep an unbounded heap -- the invisible-default shape, where the safe-looking reading of a
/// missing value is the dangerous one. A stub that forgets to seed this instead fails at its FIRST allocation,
/// loudly and immediately, where the cause is unmistakable.
const HEAP_END: *mut u32 = 0x2000_0104 as *mut u32;

/// The device GC-alloc entry the AOT emits `newobj`/`box`/array-alloc calls against:
/// `lamella_gc_alloc(payload_size [r0], &TypeDesc [r1]) -> payload* [r0]`. Bumps the fixed-RAM heap,
/// writes the descriptor at the object header (base+0), and returns the payload (base+4).
///
/// **Returns NULL when the request does not fit below [`HEAP_END`]** -- the AOT already emits a
/// null test after every allocation call, so a null is what turns exhaustion into a defined event
/// at the allocation that could not be served. Without the bound the cursor runs past the end of
/// the heap and overwrites whatever sits above it, which is not a failure a program can observe:
/// it is a program that keeps running on corrupted memory, and it presents as whatever happens to
/// occupy that address -- a statics window looks like a backend fault, a stack looks like a codegen
/// defect.
///
/// Every arithmetic step is checked. `payload_size` reaches this seam from a managed `newarr`
/// count, so a hostile or merely wrong length must not wrap the rounding (or the sum) into a
/// request that appears to fit.
#[no_mangle]
extern "C" fn lamella_gc_alloc_impl(payload_size: u32, type_desc: *const u32) -> *mut u8 {
    unsafe {
        let base = core::ptr::read_volatile(HEAP_PTR);
        let end = core::ptr::read_volatile(HEAP_END);
        let Some(rounded) = payload_size.checked_add(7).map(|n| n & !7) else {
            return core::ptr::null_mut();
        };
        let Some(next) = base.checked_add(4).and_then(|b| b.checked_add(rounded)) else {
            return core::ptr::null_mut();
        };
        if next > end {
            return core::ptr::null_mut();
        }
        core::ptr::write(base as *mut u32, type_desc as u32);
        core::ptr::write_volatile(HEAP_PTR, next);
        (base + 4) as *mut u8
    }
}

/// `abs()` of a Python typed `float`: clears the IEEE-754 sign bit. Branch-free and correct for the
/// signed-zero case (`-0.0 -> +0.0`, matching CPython), which a `x < 0.0 ? -x : x` select is not. The
/// Python frontend emits an `Inst::PInvoke` to this for `abs(<float>)` -- the lowering cannot compute
/// fabs itself (no F64 bitwise op, and its abstract-interp pass cannot emit a mid-expression branch).
#[no_mangle]
pub extern "C" fn lamella_fabs(x: f64) -> f64 {
    f64::from_bits(x.to_bits() & 0x7fff_ffff_ffff_ffff)
}

/// Round to the nearest integer, ties to EVEN (banker's rounding) -- Python's `round(<float>)` rule
/// (`round(2.5) == 2`, `round(3.5) == 4`), returned as an f64 the frontend then converts to int. Uses
/// the classic `(x + 2^52) - 2^52` identity: adding then subtracting 2^52 forces the soft-float adder's
/// default IEEE round-to-nearest-EVEN to snap `x` to an integer. A magnitude at or above 2^52 is already
/// integral (and the trick would lose bits), and NaN/inf pass through unchanged. Sign is carried onto
/// the magic constant so negatives round symmetrically. The Python frontend PInvokes this for `round`.
#[no_mangle]
pub extern "C" fn lamella_rint(x: f64) -> f64 {
    const TWO_POW_52: f64 = 4503599627370496.0;
    let magnitude = f64::from_bits(x.to_bits() & 0x7fff_ffff_ffff_ffff);
    if !(magnitude < TWO_POW_52) {
        return x;
    }
    let magic = f64::from_bits(TWO_POW_52.to_bits() | (x.to_bits() & 0x8000_0000_0000_0000));
    (x + magic) - magic
}

/// Ensure a growable Python list's backing has room for `needed_cap` elements, growing it if not:
/// `py_list_grow(header [r0], needed_cap [r1], element_size [r2])`. The Python frontend PInvokes this
/// at the head of every `append`, which is why the FIRST thing it does is return when the capacity
/// already suffices -- that check living here is what lets `append` lower without a branch of its own.
///
/// `header` is `[i32 len][i32 cap][ObjectRef backing]`; `backing` is a packed array `[u32 cap][elems]`.
/// Growing allocates a larger backing, copies the `len` live elements across, and updates `backing` and
/// `cap` in place, so every alias of the header sees the new storage. `element_size` is a parameter
/// because the backing does not record it; the caller knows it at compile time.
///
/// The new backing inherits the old one's TypeDesc (the word at `backing - 4`, per the GC ABI) rather
/// than a null or a fresh one, so a grown list stays exactly the kind of object the frontend allocated.
///
/// # Returns
///
/// **1** when the list has room for `needed_cap` -- whether it already did or was grown to fit.
/// **0** when the heap is exhausted, in which case NOTHING WAS CHANGED: the old backing, the old
/// capacity and `len` are exactly as they were, so a caller that reports the failure leaves a
/// coherent list behind rather than a half-grown one.
///
/// A caller may ignore the value and behave exactly as it did while this returned `()` -- which is
/// what lets the two halves of the fix land independently. But ignoring it re-opens the defect
/// below, so a caller that appends MUST branch on it.
///
/// **WHY THIS IS FALLIBLE AT ALL:** while
/// this seam was `void` it could not report exhaustion, so an append past a full heap wrote ONE PAST
/// the capacity it had asked for -- a memory-safety defect, not a formatting one. The alternative
/// considered was storing an exception tag here and letting the caller's existing after-call check
/// route it; that was refused because **setting a tag does not skip the store that follows**, so it
/// needed the same caller change while additionally making the only allocating seam in this archive
/// that raises. Every sibling here (`lamella_string_substring_impl`, `lamella_char_to_string_impl`,
/// `lamella_double_to_string_impl`) already reports exhaustion by RETURN VALUE; this rejoins them.
#[no_mangle]
pub extern "C" fn py_list_grow(header: *mut u32, needed_cap: u32, element_size: u32) -> u32 {
    unsafe {
        let cap = core::ptr::read(header.add(1));
        if cap >= needed_cap {
            return 1;
        }
        let new_cap = needed_cap.max(cap.saturating_mul(2));
        let len = core::ptr::read(header);
        let old_backing = core::ptr::read(header.add(2)) as *mut u32;
        let type_desc = core::ptr::read(old_backing.offset(-1)) as *const u32;
        let new_backing =
            lamella_gc_alloc_impl(4 + new_cap.saturating_mul(element_size), type_desc) as *mut u32;
        if new_backing.is_null() {
            return 0;
        }
        core::ptr::write(new_backing, new_cap);
        core::ptr::copy_nonoverlapping(
            old_backing.add(1) as *const u8,
            new_backing.add(1) as *mut u8,
            (len.saturating_mul(element_size)) as usize,
        );
        core::ptr::write(header.add(2), new_backing as u32);
        core::ptr::write(header.add(1), new_cap);
        1
    }
}

/// Python `min(a, b)` for two floats: `a` unless `b` is STRICTLY less. This is CPython's own left-to-
/// right reduce (`result = a; if b < result: result = b`), NOT IEEE `fmin` -- so `a` wins ties, a NaN
/// in `b` leaves `a` (every compare is false), and signed zero follows the plain `<` (min(-0.0, 0.0) ==
/// -0.0). Uses the same soft-float `<` (`__ltdf2`) CPython's compare does, so it is bit-for-bit CPython.
#[no_mangle]
pub extern "C" fn lamella_fmin(a: f64, b: f64) -> f64 {
    if b < a {
        b
    } else {
        a
    }
}

/// Python `max(a, b)` for two floats: `a` unless `b` is STRICTLY greater (CPython's `result = a; if b >
/// result: result = b`). Same tie/NaN/signed-zero behavior as [`lamella_fmin`], mirrored.
#[no_mangle]
pub extern "C" fn lamella_fmax(a: f64, b: f64) -> f64 {
    if b > a {
        b
    } else {
        a
    }
}

/// Semihosting `SYS_WRITEC` (0x03): write one byte to the host debug console -- QEMU stdout, or a
/// probe's console on real silicon. `r0` = the op, `r1` = a pointer to the byte; the `bkpt 0xAB` is
/// trapped by the emulator/debugger. This is the device analog of the interpreter's console seam.
///
/// `#[inline(always)]` so it folds into the exported `lamella_console_write*` -- the size-tuned LTO
/// otherwise keeps it (and `write_string`) as separate internal symbols, which the archive link then
/// leaves undefined (it pulls the exported member, not each internal callee).
#[cfg(feature = "console-semihosting")]
#[inline(always)]
fn semihost_writec(byte: u8) {
    let b = byte;
    unsafe {
        core::arch::asm!(
            "bkpt 0xAB",
            inout("r0") 0x03u32 => _,
            in("r1") &b,
            options(nostack),
        );
    }
}

#[cfg(all(feature = "console-semihosting", feature = "console-board"))]
compile_error!(
    "console_sink picks ONE destination: enable at most one of `console-semihosting` and \
     `console-board` (and `console-none` to drop the bytes)"
);
#[cfg(all(feature = "console-none", any(feature = "console-semihosting", feature = "console-board")))]
compile_error!(
    "console_sink picks ONE destination: `console-none` drops the bytes, so it cannot be combined \
     with `console-semihosting` or `console-board`"
);

/// The board's byte sink, supplied by the IMAGE rather than this archive.
///
/// A board's console is a board FACT -- which UART instance, which pins, which baud -- and this
/// archive is built once per ISA for every board, so it cannot name one. Under `console-board` the
/// image supplies this symbol and the archive routes every console byte to it. Selecting the
/// feature without providing it fails the LINK, which is the loud failure: the alternative is an
/// archive that silently picks a UART the board does not have.
#[cfg(feature = "console-board")]
unsafe extern "C" {
    fn lamella_board_console_putc(byte: u8);
}

/// ONE console byte, to whichever sink this build selected.
///
/// Every console entry point bottoms out here, and that is the point. A sink written
/// inline at every call site, so "where does console output go" had no single answer and no place
/// to bind a different one -- which is why the ARM console was semihosting-only, and why it
/// HARDFAULTS on bare silicon with no debugger attached to service the `bkpt`.
///
/// `#[inline(always)]` so it folds into the exported `lamella_console_write*` members: the
/// size-tuned LTO would otherwise leave it a separate internal symbol, which the archive link then
/// leaves undefined (it pulls the exported member, not each internal callee).
#[inline(always)]
fn console_put(byte: u8) {
    #[cfg(feature = "console-semihosting")]
    semihost_writec(byte);
    #[cfg(feature = "console-board")]
    unsafe {
        lamella_board_console_putc(byte);
    }
    #[cfg(not(any(feature = "console-semihosting", feature = "console-board")))]
    let _ = byte;
}

/// Emit one UTF-16 code unit UTF-8-encoded (BMP; a lone surrogate is emitted per-unit). The interpreter
/// likewise writes the string's code units to its console.
#[inline(always)]
fn write_unit(u: u16) {
    if u < 0x80 {
        console_put(u as u8);
    } else if u < 0x800 {
        console_put(0xC0 | (u >> 6) as u8);
        console_put(0x80 | (u & 0x3F) as u8);
    } else {
        console_put(0xE0 | (u >> 12) as u8);
        console_put(0x80 | ((u >> 6) & 0x3F) as u8);
        console_put(0x80 | (u & 0x3F) as u8);
    }
}

/// Writes a managed string's characters. `s` is the ObjectRef an `ldstr` produces -- a pointer at the
/// AOT string layout `[len: u32][u16 code units ...]`. A null string writes nothing.
#[inline(always)]
fn write_string(s: *const u32) {
    if s.is_null() {
        return;
    }
    unsafe {
        let len = core::ptr::read_volatile(s);
        let units = (s as *const u8).add(4) as *const u16;
        for i in 0..len {
            write_unit(core::ptr::read_volatile(units.add(i as usize)));
        }
    }
}

/// Powers of ten from 10^19 down to 1 (10^19 < u64::MAX), in `.rodata` -- lamella-link now lays out
/// read-only data and resolves the `.rodata` section-symbol relocation that loads this table.
static POW10: [u64; 20] = [
    10_000_000_000_000_000_000,
    1_000_000_000_000_000_000,
    100_000_000_000_000_000,
    10_000_000_000_000_000,
    1_000_000_000_000_000,
    100_000_000_000_000,
    10_000_000_000_000,
    1_000_000_000_000,
    100_000_000_000,
    10_000_000_000,
    1_000_000_000,
    100_000_000,
    10_000_000,
    1_000_000,
    100_000,
    10_000,
    1_000,
    100,
    10,
    1,
];

/// Write an unsigned decimal, most-significant digit first, by repeated SUBTRACTION of powers of ten
/// (still division-free -- a `/10` would drag in compiler_builtins' 64-bit divide). Leading zeros are
/// suppressed; `0` prints `0`.
#[inline(always)]
fn write_udec(mut v: u64) {
    let mut started = false;
    for &p in POW10.iter() {
        let mut d = 0u8;
        while v >= p {
            v -= p;
            d += 1;
        }
        if d != 0 {
            started = true;
        }
        if started || p == 1 {
            console_put(b'0' + d);
        }
    }
}

/// Write a signed decimal, a leading `-` for negatives (`unsigned_abs` so `i64::MIN` is exact).
#[inline(always)]
fn write_idec(v: i64) {
    if v < 0 {
        console_put(b'-');
        write_udec(v.unsigned_abs());
    } else {
        write_udec(v as u64);
    }
}

/// Bounds the formatted `f64` text. With G-notation (below) an extreme magnitude renders in scientific
/// form (~24 chars, e.g. `-1.7976931348623157E-308`), and an in-range value's fixed form is at most ~24
/// chars, so 64 is ample; kept generous for margin. A too-small buffer would truncate and break parity.
const F64_STR_CAP: usize = 64;

/// A `core::fmt::Write` sink into a byte slice; writes past the end are dropped (the caller sizes the buffer).
struct ByteSink<'a> {
    buf: &'a mut [u8],
    len: usize,
}
impl core::fmt::Write for ByteSink<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for &b in s.as_bytes() {
            if self.len < self.buf.len() {
                self.buf[self.len] = b;
                self.len += 1;
            }
        }
        Ok(())
    }
}

/// Parse an ASCII base-10 integer with an optional leading `-`; `None` if empty or non-digit.
fn parse_ascii_i32(bytes: &[u8]) -> Option<i32> {
    let (neg, digits) = match bytes.first() {
        Some(b'-') => (true, &bytes[1..]),
        _ => (false, bytes),
    };
    if digits.is_empty() {
        return None;
    }
    let mut v: i32 = 0;
    for &b in digits {
        if !b.is_ascii_digit() {
            return None;
        }
        v = v.checked_mul(10)?.checked_add((b - b'0') as i32)?;
    }
    Some(if neg { -v } else { v })
}

/// Formats `value` into `buf` EXACTLY as the interpreter's `format_double` does. Both spell out
/// `Infinity`/`-Infinity`; for the rest the interpreter's `apply_g_notation` chooses between the
/// non-scientific `to_string()` and .NET-style scientific `<mantissa>E<+/-><exp>` -- scientific iff the
/// base-10 exponent is `<= -5` or `>= 17`. We take the SAME decision from `core::fmt`'s `{:e}` (which, like
/// the interpreter, IS Rust's float formatting), so the device text stays byte-identical by construction --
/// including the notation switch at extreme magnitudes, which is where the two last diverged. Returns the length.
fn format_f64(value: f64, buf: &mut [u8; F64_STR_CAP]) -> usize {
    use core::fmt::Write;
    if value.is_infinite() {
        let s: &[u8] = if value < 0.0 { b"-Infinity" } else { b"Infinity" };
        buf[..s.len()].copy_from_slice(s);
        return s.len();
    }
    let mut sci = [0u8; F64_STR_CAP];
    let sci_len = {
        let mut sink = ByteSink {
            buf: &mut sci,
            len: 0,
        };
        let _ = write!(sink, "{value:e}");
        sink.len
    };
    if let Some(epos) = sci[..sci_len].iter().position(|&b| b == b'e') {
        if let Some(exp) = parse_ascii_i32(&sci[epos + 1..sci_len]) {
            if exp <= -5 || exp >= 17 {
                buf[..epos].copy_from_slice(&sci[..epos]);
                let sign = if exp < 0 { '-' } else { '+' };
                let mut sink = ByteSink { buf, len: epos };
                let _ = write!(sink, "E{sign}{:02}", exp.abs());
                return sink.len;
            }
        }
    }
    let mut sink = ByteSink { buf, len: 0 };
    let _ = write!(sink, "{value}");
    sink.len
}

/// `System.String.Substring(start, len)` on device: allocate a fresh `[len: u32][u16 units ...]` string
/// and copy `len` code units starting at index `start` from the source string `s` (the same layout an
/// `ldstr`/heap string presents). No bounds check -- an in-range call matches the interpreter exactly; the
/// .NET `ArgumentOutOfRangeException` for an out-of-range range is a program contract, not raised on device.
/// Backs `String.Substring` and, through it, `Trim`/`TrimStart`/`TrimEnd`/`Remove`.
#[no_mangle]
extern "C" fn lamella_string_substring_impl(s: *const u32, start: u32, len: u32) -> *mut u32 {
    let obj = lamella_gc_alloc_impl(4 + 2 * len, core::ptr::null()) as *mut u32;
    if obj.is_null() {
        return obj;
    }
    unsafe {
        core::ptr::write(obj, len);
        let src = (s as *const u8).add(4) as *const u16;
        let dst = (obj as *mut u8).add(4) as *mut u16;
        for i in 0..len {
            core::ptr::write(dst.add(i as usize), core::ptr::read_volatile(src.add((start + i) as usize)));
        }
    }
    obj
}

/// `System.Char.ToString()` on device: a one-unit string holding the code unit `c`.
#[no_mangle]
extern "C" fn lamella_char_to_string_impl(c: u32) -> *mut u32 {
    let obj = lamella_gc_alloc_impl(6, core::ptr::null()) as *mut u32;
    if obj.is_null() {
        return obj;
    }
    unsafe {
        core::ptr::write(obj, 1);
        core::ptr::write((obj as *mut u8).add(4) as *mut u16, c as u16);
    }
    obj
}

/// `System.Double.ToString()` on device: format `value` (byte-identical to the interpreter) and return a
/// managed string -- a GC-allocated `[len: u32][u16 code units ...]` blob, the same layout `ldstr` produces
/// and every string reader / `Console.Write(string)` consumes (each ASCII digit widens to one code unit).
/// The AOT synthesizes `Double.ToString`'s `[RuntimeProvided]` body to call this, and the managed
/// `Console.Write/WriteLine(double)` = `Write(value.ToString())` reaches it in turn. The header word at
/// `obj - 4` is a null type descriptor: the string is consumed only by the readers (which read `obj + 0`),
/// never by a virtual dispatch, exactly like a string literal's descriptor-less blob.
#[no_mangle]
extern "C" fn lamella_double_to_string_impl(value: f64) -> *mut u32 {
    let mut buf = [0u8; F64_STR_CAP];
    let len = format_f64(value, &mut buf);
    let obj = lamella_gc_alloc_impl((4 + 2 * len) as u32, core::ptr::null()) as *mut u32;
    if obj.is_null() {
        return obj;
    }
    unsafe {
        core::ptr::write(obj, len as u32);
        let units = (obj as *mut u8).add(4) as *mut u16;
        for (i, &b) in buf[..len].iter().enumerate() {
            core::ptr::write(units.add(i), b as u16);
        }
    }
    obj
}

/// The `System.Console` output primitives the AOT backend synthesizes `[RuntimeProvided]` bodies against.
/// Each writes one value with NO terminator; `WriteLine` is the value writer followed by
/// [`lamella_console_newline`]. Formatting matches the interpreter / .NET: signed/unsigned decimal,
/// `Boolean.ToString` = `True`/`False`, a `char` = its UTF-16 code unit.
#[no_mangle]
pub extern "C" fn lamella_console_write(s: *const u32) {
    write_string(s);
}

/// The ONE byte-oriented console sink: write `len` raw bytes from `ptr` to this target's debug
/// channel. The AOT's `Debug.WriteLine` / `Console.WriteLine(int)` intrinsics format front-end-side
/// and call THIS, so a new target (or a new RISC-V ISA profile) is a new implementation of the same
/// symbol with no front-end change. Byte-oriented because every console is a byte channel -- it also
/// lets one symbol carry both a literal blob and formatted digits.
///
/// Distinct from the utf16 [`lamella_console_write`] above, which stays the corlib
/// `Console.Write(string)` path over the `[len][utf16]` layout; the two are different ABIs and must
/// not share a symbol name. Both bottom out in the same [`console_put`], so which DESTINATION they
/// reach is the `console_sink` build knob's answer and never this function's: a byte seam that went
/// straight to semihosting would be a second, unknobbed console -- which is exactly what it was
/// when this and the sink knob first met in a merge, and it did not even compile without
/// `console-semihosting` selected.
#[no_mangle]
pub extern "C" fn lamella_console_write_bytes(ptr: *const u8, len: usize) {
    for i in 0..len {
        console_put(unsafe { *ptr.add(i) });
    }
}

#[no_mangle]
pub extern "C" fn lamella_console_write_i32(v: i32) {
    write_idec(v as i64);
}

#[no_mangle]
pub extern "C" fn lamella_console_write_u32(v: u32) {
    write_udec(v as u64);
}

#[no_mangle]
pub extern "C" fn lamella_console_write_i64(v: i64) {
    write_idec(v);
}

#[no_mangle]
pub extern "C" fn lamella_console_write_u64(v: u64) {
    write_udec(v);
}

#[no_mangle]
pub extern "C" fn lamella_console_write_char(v: u32) {
    write_unit(v as u16);
}

#[no_mangle]
pub extern "C" fn lamella_console_write_bool(v: i32) {
    if v != 0 {
        console_put(b'T');
        console_put(b'r');
        console_put(b'u');
        console_put(b'e');
    } else {
        console_put(b'F');
        console_put(b'a');
        console_put(b'l');
        console_put(b's');
        console_put(b'e');
    }
}

/// `'\n'` -- the interpreter's deterministic line terminator; a `WriteLine` body appends it.
#[no_mangle]
pub extern "C" fn lamella_console_newline() {
    console_put(b'\n');
}


/// Mock `recv_poll(handle, buf*, len)`: writes one canned byte (`40`) at `buf*` and reports 2 bytes read.
/// `buf*` is already the element pointer at the offset (the seam folded it), so `s.Receive(buf)` then reads
/// `buf[0] + n = 40 + 2 = 42` -- wrong unless the seam produced `arr + 4 + offset` and the `i32` return
/// marshalled back. Ignores `handle`/`len` (a real seam bounds-checks against them).
#[cfg(not(feature = "net"))]
#[no_mangle]
pub extern "C" fn lamella_net_recv_poll(_handle: i32, buf_ptr: *mut u8, _len: i32) -> i32 {
    unsafe {
        core::ptr::write_volatile(buf_ptr, 40u8);
    }
    2
}

/// Mock `send_poll(handle, buf*, len)`: accepts everything, reporting `len` bytes sent.
#[cfg(not(feature = "net"))]
#[no_mangle]
pub extern "C" fn lamella_net_send_poll(_handle: i32, _buf_ptr: *const u8, len: i32) -> i32 {
    len
}

/// Mock `Socket.ConnectStart`: accepts any address, returning handle 0 (a real seam opens the socket).
#[cfg(not(feature = "net"))]
#[no_mangle]
pub extern "C" fn lamella_net_connect_start(_addr_ptr: *const u8, _addr_len: i32, _port: i32) -> i32 {
    0
}

/// Mock `Socket.ConnectPoll`: reports connected (0 -- not WouldBlock/-1, not SockError/-2).
#[cfg(not(feature = "net"))]
#[no_mangle]
pub extern "C" fn lamella_net_connect_poll(_handle: i32) -> i32 {
    0
}

/// Mock `Socket.AcceptPoll`: reports a canned connected-socket handle (1). A real seam returns the
/// accepted socket's handle, -1 while none is pending, -2 on error.
#[cfg(not(feature = "net"))]
#[no_mangle]
pub extern "C" fn lamella_net_accept_poll(_handle: i32) -> i32 {
    1
}

/// Mock `Socket.LocalPort`: a fixed ephemeral port.
#[cfg(not(feature = "net"))]
#[no_mangle]
pub extern "C" fn lamella_net_local_port(_handle: i32) -> i32 {
    54321
}

/// Mock `Socket.CloseSocket`: no-op (a void seam).
#[cfg(not(feature = "net"))]
#[no_mangle]
pub extern "C" fn lamella_net_close(_handle: i32) {}

/// Never called -- it exists only to force the soft-float helpers a float method references into this
/// archive from `compiler_builtins`, so lamella-link can pull whichever a given program reaches:
/// `__aeabi_dmul`/`dsub`/`dadd`/`dcmpeq`/`dcmplt`, `__aeabi_fcmpeq`, and the int<->float conversions a
/// `conv.r8`/`conv.r4`/`conv.r.un` lowers to (`i2d`/`l2d`/`f2d`, `d2f`/`l2f`, unsigned `ui2d`/`ul2d`).
#[allow(clippy::eq_op)]
#[no_mangle]
pub extern "C" fn __lamella_force_softfloat(a: f64, b: f64, c: f32, i: i32, l: i64) -> i32 {
    let m = a * b;
    let s = a - b;
    let p = a + b;
    let ci = i as f64;
    let cl = l as f64;
    let cf = c as f64;
    let cd = a as f32;
    let ll = l as f32;
    let ui = (i as u32) as f64;
    let ul = (l as u64) as f64;
    ((a == b) as i32)
        + ((a < b) as i32)
        + ((c == c) as i32)
        + (m as i32)
        + (s as i32)
        + (p as i32)
        + (ci as i32)
        + (cl as i32)
        + (cf as i32)
        + (cd as i32)
        + (ll as i32)
        + (ui as i32)
        + (ul as i32)
}


/// A saved thread context: r4-r11 (callee-saved), then SP, then the resume address (LR). Laid out to
/// match [`lamella_thread_switch`]'s stores: `saved[0..8]` = r4..r11 at byte offsets 0..32, `sp` at 32,
/// `lr` at 36. A fresh thread is built with `sp` = its stack top and `lr` = its entry (Thumb bit set).
#[repr(C)]
struct ThreadContext {
    saved: [u32; 8],
    sp: u32,
    lr: u32,
}

core::arch::global_asm!(
    ".syntax unified",
    ".thumb",
    ".global lamella_thread_switch",
    ".thumb_func",
    "lamella_thread_switch:",
    "mov   r2, sp",
    "mov   r3, lr",
    "stmia r0!, {{r4-r7}}",
    "mov   r4, r8",
    "mov   r5, r9",
    "mov   r6, r10",
    "mov   r7, r11",
    "stmia r0!, {{r4-r7}}",
    "stmia r0!, {{r2-r3}}",
    "adds  r1, #16",
    "ldmia r1!, {{r4-r7}}",
    "mov   r8, r4",
    "mov   r9, r5",
    "mov   r10, r6",
    "mov   r11, r7",
    "ldmia r1!, {{r2-r3}}",
    "mov   sp, r2",
    "subs  r1, #40",
    "ldmia r1!, {{r4-r7}}",
    "bx    r3",
);

extern "C" {
    fn lamella_thread_switch(save: *mut ThreadContext, restore: *const ThreadContext);
}

const CTX_MAIN: *mut ThreadContext = 0x2000_2000 as *mut ThreadContext;
const CTX_WORKER: *mut ThreadContext = 0x2000_2040 as *mut ThreadContext;
const PING_COUNT: *mut u32 = 0x2000_2080 as *mut u32;
const WORKER_STACK_TOP: u32 = 0x2000_2500;

/// The worker green thread: on each turn bump the shared counter, then yield back to main. It never
/// returns (a green thread runs until the scheduler drops it); the loop parks via the switch.
extern "C" fn worker_entry() -> ! {
    loop {
        unsafe {
            let n = core::ptr::read_volatile(PING_COUNT);
            core::ptr::write_volatile(PING_COUNT, n.wrapping_add(1));
            lamella_thread_switch(CTX_WORKER, CTX_MAIN);
        }
    }
}

/// A self-contained proof of the context switch: spawn one worker green thread and ping-pong with it 21
/// times. `main` keeps a running `sum` LIVE across each switch (so a broken save/restore of the caller's
/// callee-saved regs corrupts it), and the worker bumps `PING_COUNT` on its own stack (so a broken SP or
/// resume address faults). Returns `sum + PING_COUNT` = 21 + 21 = **42** iff both register sets and both
/// stacks round-tripped every switch. The boot harness prints '1' on 42.
#[no_mangle]
pub extern "C" fn lamella_thread_pingpong_demo() -> u32 {
    unsafe {
        core::ptr::write_volatile(PING_COUNT, 0);
        core::ptr::write(
            CTX_WORKER,
            ThreadContext {
                saved: [0; 8],
                sp: WORKER_STACK_TOP & !7,
                lr: (worker_entry as *const () as usize as u32) | 1,
            },
        );
        let mut sum = 0u32;
        for _ in 0..21 {
            sum = sum.wrapping_add(1);
            lamella_thread_switch(CTX_MAIN, CTX_WORKER);
        }
        sum.wrapping_add(core::ptr::read_volatile(PING_COUNT))
    }
}


const MAX_THREADS: usize = 4;
/// Thread `id`'s stack top (grows down to the previous). Thread 0 (the initial/main context) keeps
/// the boot stack, so only ids 1.. use these. Fixed device RAM (like the ping-pong demo), not `.bss`.
///
/// The featureless build keeps the tight 1 KiB ladder at 0x2000_2000 -- it must fit the 16 KiB
/// micro:bit tier the M0 demos run on, and its workers only compute/yield (the mock net ops never
/// recurse). The `net` build's workers run REAL smoltcp socket ops (and possibly the reactor block
/// point) on their stacks -- measured deeper than 1 KiB, which silently overflowed into the
/// neighboring thread's parked frames -- so each gets 4 KiB in the SSE-200's reset-open SRAM above
/// the scheduler state (0x2000_3800..0x2000_6800), leaving the boot stack 6 KiB of descent from
/// 0x2000_8000.
#[cfg(not(feature = "net"))]
const fn worker_stack_top(id: usize) -> u32 {
    0x2000_2000 + (id as u32) * 0x400
}
#[cfg(feature = "net")]
const fn worker_stack_top(id: usize) -> u32 {
    0x2000_3800 + (id as u32) * 0x1000
}

/// Why a parked thread is waiting, for the reactor block point below. Mirrors
/// `lamella_cil_runtime::reactor::WaitReason` (the interp's shipping set) plus a `NotParked` slot so the
/// per-thread array needs no `Option`. A `Join`/`Monitor`-lock park is woken by hand-off, not the
/// reactor, so it is simply `NotParked` here (the runnable bit stays clear until `wake`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum ParkReason {
    /// Runnable or lock/join-blocked -- not on the reactor's timer/socket set.
    NotParked,
    /// `Thread.Sleep` until this monotonic-millisecond deadline.
    Sleep(u64),
    /// A socket op returned WouldBlock; parked until this handle is ready.
    Io(u32),
}

/// The scheduler's state: one saved context per thread, the live count, the running thread, the runnable
/// bitmap (bit `i` set = thread `i` is ready), the per-thread GC anchors, each thread's park reason (the
/// reactor's timer/socket wait set), and the C# bridge's per-thread entry pair + exit bitmap. Lives at a
/// fixed RAM address (no `.bss`).
///
/// LAYOUT CONTRACT (the anchor shims read these by offset): every field up to and including
/// `anchor_pc` is `repr(C)`-deterministic -- `contexts` = 4 x 40 bytes, so `count` is at +160,
/// `current` at +164, `anchor_sp[0]` at +172, `anchor_pc[0]` at +188 (the two arrays sit 16 bytes
/// apart). `parks` (a Rust enum, unspecified layout) and everything after it are Rust-only.
#[repr(C)]
struct Scheduler {
    contexts: [ThreadContext; MAX_THREADS],
    count: u32,
    current: u32,
    runnable: u32,
    /// The managed-frame ANCHOR the GC stack walk starts from (design sec 2.3 refinement 1): the
    /// SP at the thread's last managed->runtime seam call. Written by the asm entry shim of every
    /// anchor seam ([`ANCHOR-SEAM shims`](self) below); a switched-away thread's SAVED context PC
    /// points into Rust code, so the walk must start here instead.
    anchor_sp: [u32; MAX_THREADS],
    /// The anchor's PC twin: the seam call's return address -- a PC inside the innermost MANAGED
    /// frame (the caller of the seam), which the walker matches against a method record. 0 = the
    /// thread has never crossed an anchor seam (no managed frames to walk; its entry delegate is
    /// rooted globally via `entry_args`).
    anchor_pc: [u32; MAX_THREADS],
    parks: [ParkReason; MAX_THREADS],
    /// A C#-spawned thread's compiled managed entry helper (`Thread.ThreadEntry`'s code address) ...
    entries: [u32; MAX_THREADS],
    /// ... and the ThreadStart delegate it invokes (an object pointer, opaque to this side).
    entry_args: [u32; MAX_THREADS],
    /// Bit `i` set = thread `i` has exited -- what `lamella_thread_join` waits on.
    done: u32,
}
const SCHED: *mut Scheduler = 0x2000_3000 as *mut Scheduler;
const SCHED_COUNTER: *mut u32 = 0x2000_3400 as *mut u32;

macro_rules! anchor_seam_shim {
    ($name:literal) => {
        core::arch::global_asm!(
            ".syntax unified",
            ".thumb",
            concat!(".global ", $name),
            ".thumb_func",
            concat!($name, ":"),
            "push {{r0-r3}}",
            "ldr  r0, =0x200030A4",
            "ldr  r0, [r0]",
            "lsls r0, r0, #2",
            "ldr  r1, =0x200030AC",
            "adds r1, r0",
            "mov  r2, sp",
            "adds r2, #16",
            "str  r2, [r1, #0]",
            "mov  r2, lr",
            "str  r2, [r1, #16]",
            concat!("ldr  r3, =", $name, "_impl"),
            "movs r0, #1",
            "orrs r3, r0",
            "mov  r12, r3",
            "pop  {{r0-r3}}",
            "bx   r12",
            ".ltorg",
        );
    };
}

anchor_seam_shim!("lamella_thread_yield");
anchor_seam_shim!("lamella_thread_join");
anchor_seam_shim!("lamella_thread_sleep");
anchor_seam_shim!("lamella_thread_connect_poll");
anchor_seam_shim!("lamella_thread_accept_poll");
anchor_seam_shim!("lamella_thread_send_poll");
anchor_seam_shim!("lamella_thread_recv_poll");
anchor_seam_shim!("lamella_gc_alloc");
anchor_seam_shim!("lamella_monitor_enter");
anchor_seam_shim!("lamella_monitor_wait");
anchor_seam_shim!("lamella_string_substring");
anchor_seam_shim!("lamella_char_to_string");
anchor_seam_shim!("lamella_double_to_string");
anchor_seam_shim!("lamella_gc_walk_roots");
anchor_seam_shim!("lamella_gc_count_roots");

/// The next runnable thread after `from`, round-robin; `from` itself if nothing else is ready.
fn sched_next_runnable(s: &Scheduler, from: usize) -> usize {
    let n = s.count as usize;
    let mut next = from;
    for _ in 0..MAX_THREADS {
        next = (next + 1) % n;
        if s.runnable & (1u32 << next) != 0 {
            return next;
        }
    }
    from
}

/// Yield the CPU: pick the next runnable thread and switch to it (a no-op if the caller is the only one
/// runnable). The caller resumes here on its next turn, its callee-saved state restored by the switch.
unsafe fn sched_yield() {
    let s = &mut *SCHED;
    let from = s.current as usize;
    let next = sched_next_runnable(s, from);
    if next == from {
        return;
    }
    s.current = next as u32;
    lamella_thread_switch(&mut s.contexts[from], &s.contexts[next]);
}

/// End the current thread: clear its runnable bit, set its done bit (what a Join waits on), and switch
/// away for good (its context is abandoned into a scratch save nothing reads). With nothing else
/// runnable the exiting thread drives the reactor block point until a parked thread wakes (its own
/// stack hosts that wait -- still valid, nothing reclaims stacks; switching to a stale saved context
/// instead would re-run dead code). Never returns -- a cleared thread is never scheduled again.
unsafe fn sched_exit() -> ! {
    let s = &mut *SCHED;
    let cur = s.current as usize;
    s.runnable &= !(1u32 << cur);
    s.done |= 1u32 << cur;
    loop {
        if s.runnable != 0 {
            let next = sched_next_runnable(s, cur);
            s.current = next as u32;
            let mut dead = ThreadContext {
                saved: [0; 8],
                sp: 0,
                lr: 0,
            };
            lamella_thread_switch(&mut dead, &s.contexts[next]);
            loop {}
        }
        if !sched_block_point(s) {
            sched_deadlock_trap();
        }
    }
}

/// Start a green thread running `entry` on its own fresh stack; add it to the ready set. The new
/// slot's GC anchor is cleared here (NOT in `sched_ensure_init`, which runs INSIDE an anchor seam
/// -- after the entry shim already wrote the calling thread's anchor): a spawned-never-run thread
/// has no managed frames, and anchor_pc 0 is how the root walk knows to skip its stack.
unsafe fn sched_spawn(entry: extern "C" fn() -> !) {
    let s = &mut *SCHED;
    let id = s.count as usize;
    core::ptr::write(
        &mut s.contexts[id],
        ThreadContext {
            saved: [0; 8],
            sp: worker_stack_top(id) & !7,
            lr: (entry as *const () as usize as u32) | 1,
        },
    );
    s.anchor_sp[id] = 0;
    s.anchor_pc[id] = 0;
    s.runnable |= 1u32 << id;
    s.count += 1;
}

/// A worker green thread: bump the shared counter by 2 and yield, seven times, then exit. The loop index
/// is per-thread state carried across each yield (so a broken save/restore corrupts the total).
extern "C" fn sched_worker() -> ! {
    unsafe {
        let mut i = 0u32;
        while i < 7 {
            let c = core::ptr::read_volatile(SCHED_COUNTER);
            core::ptr::write_volatile(SCHED_COUNTER, c.wrapping_add(2));
            sched_yield();
            i += 1;
        }
        sched_exit();
    }
}

/// A self-contained proof of the cooperative scheduler: spawn THREE worker green threads and round-robin
/// with them until all have exited. Each worker bumps the shared counter 7 times by 2 on its own stack, so
/// the total is 3 * 7 * 2 = **42** iff spawn, the round-robin ready queue, per-thread stacks, per-thread
/// callee-saved state across yields, and thread exit all work. The boot harness prints '1' on 42.
#[no_mangle]
pub extern "C" fn lamella_sched_demo() -> u32 {
    unsafe {
        let s = &mut *SCHED;
        s.count = 1;
        s.current = 0;
        s.runnable = 1;
        core::ptr::write_volatile(SCHED_COUNTER, 0);
        sched_spawn(sched_worker);
        sched_spawn(sched_worker);
        sched_spawn(sched_worker);
        while (*SCHED).runnable != 1 {
            sched_yield();
        }
        core::ptr::read_volatile(SCHED_COUNTER)
    }
}


/// Mock monotonic clock (ms) for the self-contained reactor demo; the `net` build reads the real clock seam.
#[cfg(not(feature = "net"))]
const REACTOR_NOW: *mut u64 = 0x2000_3800 as *mut u64;
/// The one socket handle the mock poll reports ready (0 = none); the `net` build calls the real network poll.
#[cfg(not(feature = "net"))]
const REACTOR_NET_READY: *mut u32 = 0x2000_3808 as *mut u32;

/// The `net` reactor environment: the real C-ABI seams over the installed network stack
/// (lamella-runtime-support-net). One monotonic clock -- `lamella_net_now_ms` forwards to the SAME
/// `now_ms` the firmware handed the net backend -- and the single blocking wait is `lamella_net_poll`
/// into a caller stack buffer (`timeout_ms < 0` = block until ready; ready handles past the buffer stay
/// registered and re-report next call, so none is lost). A `0` clock reading means none is registered
/// yet; the block point then treats a timer as already due (the canonical no-clock degradation).
#[cfg(feature = "net")]
mod reactor_env {
    use super::net_support;

    pub fn now_millis() -> Option<u64> {
        let now = net_support::lamella_net_now_ms();
        (now != 0).then_some(now)
    }

    pub fn sleep_millis(millis: u64) {
        unsafe {
            net_support::lamella_net_poll(clamp_timeout(millis), core::ptr::null_mut(), 0);
        }
    }

    pub fn net_poll_into(timeout: Option<u64>, ready: &mut [u32]) -> usize {
        let timeout_ms = timeout.map_or(-1, clamp_timeout);
        let count = unsafe {
            net_support::lamella_net_poll(timeout_ms, ready.as_mut_ptr(), ready.len() as u32)
        };
        if count <= 0 { 0 } else { count as usize }
    }

    pub fn net_deregister(handle: u32) {
        net_support::lamella_net_deregister(handle as i32);
    }

    fn clamp_timeout(millis: u64) -> i64 {
        millis.min(i64::MAX as u64) as i64
    }
}

/// The featureless (mock) reactor environment over [`REACTOR_NOW`]/[`REACTOR_NET_READY`], for the
/// self-contained reactor demo -- no clock hardware, no net stack. The mock "wait" ADVANCES the clock
/// by the timeout (there is no real time source), so a timed park deterministically wakes.
#[cfg(not(feature = "net"))]
mod reactor_env {
    use super::{REACTOR_NET_READY, REACTOR_NOW};

    pub fn now_millis() -> Option<u64> {
        Some(unsafe { core::ptr::read_volatile(REACTOR_NOW) })
    }

    pub fn sleep_millis(millis: u64) {
        unsafe {
            let now = core::ptr::read_volatile(REACTOR_NOW);
            core::ptr::write_volatile(REACTOR_NOW, now + millis);
        }
    }

    pub fn net_poll_into(timeout: Option<u64>, ready: &mut [u32]) -> usize {
        if let Some(millis) = timeout {
            sleep_millis(millis);
        }
        let handle = unsafe { core::ptr::read_volatile(REACTOR_NET_READY) };
        if handle != 0 && !ready.is_empty() {
            ready[0] = handle;
            1
        } else {
            0
        }
    }

    pub fn net_deregister(_handle: u32) {}
}

/// True if any live thread is parked on the reactor's timer/socket set (so the block point has work).
#[cfg(not(feature = "net"))]
fn sched_any_parked(s: &Scheduler) -> bool {
    s.parks[..s.count as usize].iter().any(|p| *p != ParkReason::NotParked)
}

/// A parked/exiting/joining thread found NOTHING runnable and the block point NOTHING external to wait
/// on: every live thread is blocked on a join/lock nobody can satisfy. Fail LOUD -- print a marker on
/// the semihosting console (visibly wrong next to the harnesses' single-'0'/'1' protocol) and halt,
/// rather than silently spinning a scheduler that can never make progress again.
fn sched_deadlock_trap() -> ! {
    for b in *b"DEADLOCK" {
        console_put(b);
    }
    loop {}
}

/// Park the current thread on `reason`: clear its runnable bit, record the reason, and run others
/// through the shared re-entry loop. The thread resumes here once woken and rescheduled -- by the
/// block point's wake pass (its `reason` came due) or, single-threaded, by its own wait completing.
unsafe fn sched_park(reason: ParkReason) {
    let s = &mut *SCHED;
    let cur = s.current as usize;
    s.parks[cur] = reason;
    s.runnable &= !(1u32 << cur);
    sched_block_current(s);
}

/// The blocked-thread re-entry loop shared by a reactor park ([`sched_park`]) and a Monitor block
/// ([`sched_block_for_handoff`]). The caller has already cleared the current thread's runnable
/// bit; run the next runnable thread until a wake sets the bit again (a self-wake with nothing
/// else runnable is a plain return), and with NOTHING runnable drive the reactor block point --
/// possibly on behalf of threads parked on timers/sockets while the current one awaits a hand-off.
/// A false block point (nothing external to wait on) is the lock/join deadlock: fail loud.
unsafe fn sched_block_current(s: &mut Scheduler) {
    let cur = s.current as usize;
    loop {
        if s.runnable != 0 {
            let next = sched_next_runnable(s, cur);
            if next == cur {
                return;
            }
            s.current = next as u32;
            lamella_thread_switch(&mut s.contexts[cur], &s.contexts[next]);
            return;
        }
        if !sched_block_point(s) {
            sched_deadlock_trap();
        }
    }
}

/// Block the current thread for a Monitor hand-off: clear its runnable bit and run others (or the
/// block point) until the releasing owner wakes it ([`sched_wake`]). `parks[cur]` stays
/// `NotParked` -- a lock park is woken by hand-off, NOT the reactor (design sec 1.1) -- which is
/// exactly what lets a pure lock cycle fall through the block point's nothing-external rule into
/// the deadlock trap.
unsafe fn sched_block_for_handoff() {
    let s = &mut *SCHED;
    let cur = s.current as usize;
    s.runnable &= !(1u32 << cur);
    sched_block_current(s);
}

/// The Monitor hand-off wake (the interp scheduler's `WakeThread` twin): make thread `id` runnable
/// again. A lock-blocked thread is `NotParked`, so only the runnable bit changes.
unsafe fn sched_wake(id: usize) {
    (*SCHED).runnable |= 1u32 << id;
}

/// The most ready sockets one block point drains (its fixed stack poll buffer) -- the no-alloc
/// counterpart of the canonical `READY_BATCH`, sized to this scheduler's small thread table (at most
/// [`MAX_THREADS`] io-waiters). Handles past it stay registered and re-report next call; none is lost.
const READY_BATCH: usize = 8;

/// The single block point: with no thread runnable, block ONCE on the nearest timer deadline and/or the
/// socket poll, then mark the due sleepers + ready io-waiters runnable. Returns `false` when nothing is
/// waited on (the remaining threads are lock/join-deadlocked and the caller must stop --
/// [`sched_deadlock_trap`]). A `true` return may wake NOBODY (the poll timed out or drained early);
/// the caller loops. Faithful no-alloc twin of `reactor::block_point_into` (the CANONICAL reference)
/// over the fixed park array + the [`reactor_env`] seams.
unsafe fn sched_block_point(s: &mut Scheduler) -> bool {
    let mut nearest_deadline: Option<u64> = None;
    let mut any_io = false;
    for i in 0..s.count as usize {
        match s.parks[i] {
            ParkReason::Sleep(deadline) => {
                nearest_deadline = Some(nearest_deadline.map_or(deadline, |n| n.min(deadline)));
            }
            ParkReason::Io(_) => any_io = true,
            ParkReason::NotParked => {}
        }
    }
    if nearest_deadline.is_none() && !any_io {
        return false;
    }
    let timeout = match (nearest_deadline, reactor_env::now_millis()) {
        (Some(deadline), Some(now)) => Some(deadline.saturating_sub(now)),
        (Some(_), None) => Some(0),
        (None, _) => None,
    };
    let mut ready_buf = [0u32; READY_BATCH];
    let ready: &[u32] = if any_io {
        let n = reactor_env::net_poll_into(timeout, &mut ready_buf);
        &ready_buf[..n]
    } else {
        if let Some(millis) = timeout {
            reactor_env::sleep_millis(millis);
        }
        &ready_buf[..0]
    };
    let now = reactor_env::now_millis().unwrap_or(u64::MAX);
    for i in 0..s.count as usize {
        let wake = match s.parks[i] {
            ParkReason::Sleep(deadline) => deadline <= now,
            ParkReason::Io(handle) => {
                let hit = ready.contains(&handle);
                if hit {
                    reactor_env::net_deregister(handle);
                }
                hit
            }
            ParkReason::NotParked => false,
        };
        if wake {
            s.parks[i] = ParkReason::NotParked;
            s.runnable |= 1u32 << i;
        }
    }
    true
}

/// A reactor worker: bump the shared counter by 14, PARK on `reason`, and -- once the block point wakes it
/// -- bump by 7 more and exit. The after-wake bump happens ONLY if park + block point + wake round-trip.
#[cfg(not(feature = "net"))]
unsafe fn reactor_worker(reason: ParkReason) -> ! {
    let c = core::ptr::read_volatile(SCHED_COUNTER);
    core::ptr::write_volatile(SCHED_COUNTER, c.wrapping_add(14));
    sched_park(reason);
    let c = core::ptr::read_volatile(SCHED_COUNTER);
    core::ptr::write_volatile(SCHED_COUNTER, c.wrapping_add(7));
    sched_exit();
}

#[cfg(not(feature = "net"))]
extern "C" fn reactor_worker_io() -> ! {
    unsafe { reactor_worker(ParkReason::Io(7)) }
}
#[cfg(not(feature = "net"))]
extern "C" fn reactor_worker_sleep() -> ! {
    unsafe { reactor_worker(ParkReason::Sleep(50)) }
}

/// A self-contained proof of the PARKING half + the reactor block point: spawn two workers -- one parks on
/// a SOCKET (Io), one on a TIMER (Sleep) -- so `runnable` drops to just `main` with both parked. `main` then
/// reaches the block point, which (mock clock + mock ready socket) wakes BOTH in the one wait: the socket
/// readies and the timer fires. Each worker bumps the counter 14 before parking and 7 after waking, so the
/// total is 2*(14+7) = 42 iff park, the block point (nearest-deadline + socket-poll + wake pass), and the
/// resume-after-wake all work. The boot harness prints '1' on 42. Featureless only: the demo scripts the
/// MOCK reactor env; the `net` build's block point reads the real clock/poll seams instead.
#[cfg(not(feature = "net"))]
#[no_mangle]
pub extern "C" fn lamella_reactor_demo() -> u32 {
    unsafe {
        (*SCHED).count = 1;
        (*SCHED).current = 0;
        (*SCHED).runnable = 1;
        for i in 0..MAX_THREADS {
            (*SCHED).parks[i] = ParkReason::NotParked;
        }
        core::ptr::write_volatile(SCHED_COUNTER, 0);
        core::ptr::write_volatile(REACTOR_NOW, 0);
        core::ptr::write_volatile(REACTOR_NET_READY, 7);
        sched_spawn(reactor_worker_io);
        sched_spawn(reactor_worker_sleep);
        loop {
            if (*SCHED).runnable & !1u32 != 0 {
                sched_yield();
            } else if sched_any_parked(&*SCHED) {
                if !sched_block_point(&mut *SCHED) {
                    break;
                }
            } else {
                break;
            }
        }
        core::ptr::read_volatile(SCHED_COUNTER)
    }
}


/// The generic native entry of every C#-spawned thread: call the slot's managed entry helper with its
/// delegate on this thread's fresh stack, then exit through the scheduler. Runs as `SCHED.current`
/// (the switch that first ran this thread set `current` to its slot).
extern "C" fn thread_entry_trampoline() -> ! {
    unsafe {
        let cur = (*SCHED).current as usize;
        let entry: extern "C" fn(u32) =
            core::mem::transmute(((*SCHED).entries[cur] | 1) as usize);
        let arg = (*SCHED).entry_args[cur];
        entry(arg);
        sched_exit();
    }
}

/// First-use scheduler init: the CALLER becomes thread 0 on the boot stack. QEMU zeroes RAM, so
/// `count == 0` marks "never used"; a real-silicon boot path zeroes SCHED explicitly before managed
/// code. Idempotent -- a later call with threads live is a no-op. Every seam that can spawn OR park
/// calls this first, so a single-threaded program's very first park finds a coherent scheduler.
unsafe fn sched_ensure_init() {
    let s = &mut *SCHED;
    if s.count == 0 {
        s.count = 1;
        s.current = 0;
        s.runnable = 1;
        s.done = 0;
        for i in 0..MAX_THREADS {
            s.parks[i] = ParkReason::NotParked;
        }
    }
}

/// C# `Thread.Start` -> spawn a green thread running the managed `entry(delegate)`. Returns the new
/// thread's scheduler id (its slot, always >= 1), or -1 when the fixed thread table is full.
/// `is_background` is accepted but unused: Tier 1 has no daemon-abandon semantics.
#[no_mangle]
pub extern "C" fn lamella_thread_start(entry: u32, delegate: u32, _is_background: i32) -> i32 {
    unsafe {
        sched_ensure_init();
        let s = &mut *SCHED;
        let id = s.count as usize;
        if id >= MAX_THREADS {
            return -1;
        }
        s.entries[id] = entry;
        s.entry_args[id] = delegate;
        sched_spawn(thread_entry_trampoline);
        id as i32
    }
}

/// C# `Thread.Yield` -> one cooperative round-robin turn. A no-op before any spawn (the scheduler is
/// uninitialized and there is nothing else to run).
#[no_mangle]
extern "C" fn lamella_thread_yield_impl() {
    unsafe {
        if (*SCHED).count > 1 {
            sched_yield();
        }
    }
}

/// C# `Thread.Join(id)` -> run the other threads until thread `id` exits (its `done` bit sets). The
/// joiner stays RUNNABLE and donates its turn while anything else can run (the cooperative
/// busy-join); with everything else PARKED it drives the reactor block point instead -- so joining a
/// thread that is asleep or blocked on a socket waits in the ONE real wait rather than spinning a
/// no-op yield forever. A false block point (nothing external left to wait on) means the joined
/// thread can never finish: fail loud. An id no spawn returned returns at once rather than hanging.
#[no_mangle]
extern "C" fn lamella_thread_join_impl(id: i32) {
    unsafe {
        if id < 1 || id as u32 >= (*SCHED).count {
            return;
        }
        while (*SCHED).done & (1u32 << id) == 0 {
            let s = &mut *SCHED;
            let cur = s.current as usize;
            if sched_next_runnable(s, cur) != cur {
                sched_yield();
            } else if !sched_block_point(s) {
                sched_deadlock_trap();
            }
        }
    }
}

/// C# `Thread.Sleep(ms)` on the `net` build (Tier 2): a REAL timed park -- `Sleep(now + ms)` on the
/// reactor against the `lamella_net_now_ms` clock (the SAME monotonic clock the net stack runs on);
/// the block point's timed wait wakes it. With no clock registered yet (or a non-positive `ms`),
/// degrade to one cooperative yield -- the managed surface's documented no-clock behavior.
#[cfg(feature = "net")]
#[no_mangle]
extern "C" fn lamella_thread_sleep_impl(milliseconds: i32) {
    if milliseconds > 0 {
        if let Some(now) = reactor_env::now_millis() {
            unsafe {
                sched_ensure_init();
                sched_park(ParkReason::Sleep(now.saturating_add(milliseconds as u64)));
            }
            return;
        }
    }
    lamella_thread_yield_impl();
}

/// C# `Thread.Sleep(ms)` on the featureless build: no board clock is wired to this scheduler, so a
/// timed park could never truly wait; degrade to one cooperative yield (the managed surface's
/// documented no-clock behavior). The `net` build parks for real against `lamella_net_now_ms`.
#[cfg(not(feature = "net"))]
#[no_mangle]
extern "C" fn lamella_thread_sleep_impl(_milliseconds: i32) {
    lamella_thread_yield_impl();
}


/// The WouldBlock sentinel the raw ops report (and the corlib bakes in) -- the one value the parking
/// wrappers never return.
const WOULD_BLOCK: i32 = -1;

#[cfg(feature = "net")]
fn raw_connect_poll(handle: i32) -> i32 {
    net_support::lamella_net_connect_poll(handle)
}
#[cfg(feature = "net")]
fn raw_accept_poll(handle: i32) -> i32 {
    net_support::lamella_net_accept_poll(handle)
}
#[cfg(feature = "net")]
fn raw_send_poll(handle: i32, buf: *const u8, len: u32) -> i32 {
    unsafe { net_support::lamella_net_send_poll(handle, buf, len) }
}
#[cfg(feature = "net")]
fn raw_recv_poll(handle: i32, buf: *mut u8, len: u32) -> i32 {
    unsafe { net_support::lamella_net_recv_poll(handle, buf, len) }
}
#[cfg(not(feature = "net"))]
fn raw_connect_poll(handle: i32) -> i32 {
    lamella_net_connect_poll(handle)
}
#[cfg(not(feature = "net"))]
fn raw_accept_poll(handle: i32) -> i32 {
    lamella_net_accept_poll(handle)
}
#[cfg(not(feature = "net"))]
fn raw_send_poll(handle: i32, buf: *const u8, len: u32) -> i32 {
    lamella_net_send_poll(handle, buf, len as i32)
}
#[cfg(not(feature = "net"))]
fn raw_recv_poll(handle: i32, buf: *mut u8, len: u32) -> i32 {
    lamella_net_recv_poll(handle, buf, len as i32)
}

/// Park the current thread on `handle` until the reactor wakes it (the would-blocked raw op already
/// registered the interest). Lazily initializes the scheduler so a single-threaded program -- no
/// `Thread.Start` ever called -- parks straight into the block point's one real wait.
unsafe fn sched_park_io(handle: i32) {
    sched_ensure_init();
    sched_park(ParkReason::Io(handle as u32));
}

/// C# `Socket.ConnectPoll` (parking): 0 connected, -2 failed; a would-block parks until writable.
#[no_mangle]
extern "C" fn lamella_thread_connect_poll_impl(handle: i32) -> i32 {
    loop {
        let result = raw_connect_poll(handle);
        if result != WOULD_BLOCK {
            return result;
        }
        unsafe { sched_park_io(handle) };
    }
}

/// C# `Socket.AcceptPoll` (parking): a connected socket handle, or -2; a would-block parks until a
/// connection is pending.
#[no_mangle]
extern "C" fn lamella_thread_accept_poll_impl(handle: i32) -> i32 {
    loop {
        let result = raw_accept_poll(handle);
        if result != WOULD_BLOCK {
            return result;
        }
        unsafe { sched_park_io(handle) };
    }
}

/// C# `Socket.SendPoll` (parking; the seam pre-offsets the slice): bytes accepted, or -2; a
/// would-block parks until the socket is writable.
#[no_mangle]
extern "C" fn lamella_thread_send_poll_impl(handle: i32, buf: *const u8, len: u32) -> i32 {
    loop {
        let result = raw_send_poll(handle, buf, len);
        if result != WOULD_BLOCK {
            return result;
        }
        unsafe { sched_park_io(handle) };
    }
}

/// C# `Socket.ReceivePoll` (parking; the seam pre-offsets the slice): bytes read (0 = clean close),
/// or -2; a would-block parks until the socket is readable.
#[no_mangle]
extern "C" fn lamella_thread_recv_poll_impl(handle: i32, buf: *mut u8, len: u32) -> i32 {
    loop {
        let result = raw_recv_poll(handle, buf, len);
        if result != WOULD_BLOCK {
            return result;
        }
        unsafe { sched_park_io(handle) };
    }
}


/// Fixed capacity of DISTINCT objects concurrently locked or awaited. Exceeding it fails loud
/// ([`monitor_table_full_trap`]) rather than silently dropping mutual exclusion.
const MAX_LOCKS: usize = 8;

/// One per-object lock: the interp `LockState`'s fixed-RAM twin.
#[repr(C)]
struct LockEntry {
    /// The locked object's address (the managed reference, marshalled RefToInt); 0 = a free slot.
    /// Keying on the ADDRESS makes every locked object part of the GC pin/relocate contract, the
    /// same family as `SCHED.entry_args`.
    obj: u32,
    /// The owning thread id; meaningful only while `recursion != 0`.
    owner: u32,
    /// The owner's Enter depth (a recursive `lock` re-enters); 0 = unowned, free to take -- the
    /// entry then lives only for its `wait_set`.
    recursion: u32,
    /// Threads blocked in Enter (bit = thread id), each woken by a release's hand-off.
    waiters: u32,
    /// Threads parked in `Monitor.Wait` (bit = thread id), each moved to `waiters` by a Pulse.
    wait_set: u32,
}

/// The lock table plus the per-thread depth a hand-off restores: 1 for an Enter contender, the
/// saved Enter depth for a `Monitor.Wait` re-acquire (the interp `LockWaiter.depth`'s twin).
#[repr(C)]
struct LockTable {
    entries: [LockEntry; MAX_LOCKS],
    grant_depth: [u32; MAX_THREADS],
}

/// Fixed device RAM directly after [`SCHED_COUNTER`], placed rather than `.bss`: 176 bytes at
/// 0x2000_3410..0x2000_34C0, clear of the featureless mock reactor cells (0x2000_3800+) and the
/// `net` build's worker stacks (0x2000_3800..0x2000_6800). QEMU zeroes RAM (= every slot free);
/// a silicon boot zeroes it alongside SCHED.
const LOCKS: *mut LockTable = 0x2000_3410 as *mut LockTable;

/// More than [`MAX_LOCKS`] DISTINCT objects locked/awaited at once: the fixed table cannot grow,
/// and proceeding without a slot would drop mutual exclusion. Prints `LOCKFULL` over semihosting
/// (visibly wrong next to the harnesses' single-'0'/'1' protocol) and halts.
fn monitor_table_full_trap() -> ! {
    for b in *b"LOCKFULL" {
        console_put(b);
    }
    loop {}
}

/// `Monitor.Wait`/`Pulse`/`PulseAll` by a thread that does not own the lock -- the interpreter's
/// `SynchronizationLockException` site. A native seam cannot raise a managed exception, so fail
/// LOUD (`MONITOR` + halt) rather than silently diverge from the interp on an erroneous program.
/// (A non-owner `Exit` is different: the interp releases nothing and stays silent -- mirrored.)
fn monitor_not_owner_trap() -> ! {
    for b in *b"MONITOR" {
        console_put(b);
    }
    loop {}
}

/// `Monitor.Enter(null)`/`TryEnter(null)` -- the interpreter traps LOUD (`Trap::TypeMismatch` on a
/// non-Object), but this table would ACCEPT key 0 by accident: 0 is also the free-slot sentinel,
/// so a held/awaited null lock could be clobbered by any later new-slot scan. Fail loud instead
/// (`NULLLOCK` + halt; the RISC-V twin exits 5). Wait/Pulse on null fall into
/// [`monitor_not_owner_trap`] already -- no 0-keyed entry can exist past this guard.
fn monitor_null_trap() -> ! {
    for b in *b"NULLLOCK" {
        console_put(b);
    }
    loop {}
}

/// The interp's `lock_try`: take a free slot or an unowned entry, or bump the owner's recursion;
/// `false` on contention (NO enqueue). Fails loud when a NEW entry is needed and the table is full
/// -- also for `TryEnter`, where returning 0 would misreport a capacity failure as contention.
unsafe fn lock_try_acquire(obj: u32, me: u32) -> bool {
    let t = &mut *LOCKS;
    for e in t.entries.iter_mut() {
        if e.obj == obj {
            if e.recursion == 0 {
                e.owner = me;
                e.recursion = 1;
                return true;
            }
            if e.owner == me {
                e.recursion += 1;
                return true;
            }
            return false;
        }
    }
    for e in t.entries.iter_mut() {
        if e.obj == 0 {
            *e = LockEntry {
                obj,
                owner: me,
                recursion: 1,
                waiters: 0,
                wait_set: 0,
            };
            return true;
        }
    }
    monitor_table_full_trap()
}

/// The outermost release (the interp `lock_release`'s tail): hand the lock to the lowest-id
/// Enter-waiter -- owner + its grant depth restored BEFORE it can run -- and WAKE it (the hand-off
/// wake); else leave the lock unowned, freeing the slot only when no `Monitor.Wait`-er keeps the
/// entry alive.
unsafe fn lock_release_outermost(t: &mut LockTable, i: usize) {
    let waiters = t.entries[i].waiters;
    if waiters != 0 {
        let w = waiters.trailing_zeros() as usize;
        t.entries[i].waiters &= !(1u32 << w);
        t.entries[i].owner = w as u32;
        t.entries[i].recursion = t.grant_depth[w];
        sched_wake(w);
    } else {
        t.entries[i].recursion = 0;
        if t.entries[i].wait_set == 0 {
            t.entries[i].obj = 0;
        }
    }
}

/// Whether `me` holds the lock on `obj` right now -- what a woken contender re-checks (it may only
/// run after further hand-offs) and the Wait/Pulse ownership precondition.
unsafe fn lock_owned_by(obj: u32, me: u32) -> bool {
    (*LOCKS)
        .entries
        .iter()
        .any(|e| e.obj == obj && e.recursion != 0 && e.owner == me)
}

/// C# `Monitor.Enter(obj)`: acquire the object's lock for the running thread, BLOCKING on
/// contention. The uncontended/recursive path is one table scan; a contender enrolls in the
/// entry's `waiters` and blocks until the release hand-off wakes it already owning the lock.
#[no_mangle]
extern "C" fn lamella_monitor_enter_impl(obj: u32) {
    if obj == 0 {
        monitor_null_trap();
    }
    unsafe {
        sched_ensure_init();
        let me = (*SCHED).current;
        loop {
            if lock_try_acquire(obj, me) {
                return;
            }
            {
                let t = &mut *LOCKS;
                t.grant_depth[me as usize] = 1;
                if let Some(e) = t.entries.iter_mut().find(|e| e.obj == obj) {
                    e.waiters |= 1u32 << me;
                }
            }
            sched_block_for_handoff();
            if lock_owned_by(obj, me) {
                return;
            }
        }
    }
}

/// C# `Monitor.Exit(obj)`: release one level; the outermost release hands the lock to the
/// lowest-id waiter and wakes it. A non-owner (or never-entered) Exit is a silent no-op -- the
/// interp `lock_release`'s `None` path.
#[no_mangle]
pub extern "C" fn lamella_monitor_exit(obj: u32) {
    unsafe {
        sched_ensure_init();
        let me = (*SCHED).current;
        let t = &mut *LOCKS;
        let Some(i) = t.entries.iter().position(|e| e.obj == obj) else {
            return;
        };
        if t.entries[i].recursion == 0 || t.entries[i].owner != me {
            return;
        }
        t.entries[i].recursion -= 1;
        if t.entries[i].recursion == 0 {
            lock_release_outermost(t, i);
        }
    }
}

/// C# `Monitor.TryEnter(obj)`: the non-blocking arm -- 1 if the lock is now held by this thread
/// (free, unowned, or recursive), 0 on contention with NO enqueue (the interp's `lock_try`).
#[no_mangle]
pub extern "C" fn lamella_monitor_try_enter(obj: u32) -> i32 {
    if obj == 0 {
        monitor_null_trap();
    }
    unsafe {
        sched_ensure_init();
        let me = (*SCHED).current;
        i32::from(lock_try_acquire(obj, me))
    }
}

/// C# `Monitor.Wait(obj)`: park in the object's condition wait-set at the CURRENT recursion depth
/// and FULLY release the lock (hand-off if contended; the entry survives -- its wait-set is
/// nonempty), then block until a Pulse moves us to the acquire set AND a later release hands the
/// lock back at the saved depth. Ownership is the precondition (the interp's
/// `SynchronizationLockException` site; this seam fails loud instead).
#[no_mangle]
extern "C" fn lamella_monitor_wait_impl(obj: u32) {
    unsafe {
        sched_ensure_init();
        let me = (*SCHED).current;
        {
            let t = &mut *LOCKS;
            let Some(i) = t.entries.iter().position(|e| e.obj == obj) else {
                monitor_not_owner_trap();
            };
            if t.entries[i].recursion == 0 || t.entries[i].owner != me {
                monitor_not_owner_trap();
            }
            t.grant_depth[me as usize] = t.entries[i].recursion;
            t.entries[i].wait_set |= 1u32 << me;
            lock_release_outermost(t, i);
        }
        loop {
            sched_block_for_handoff();
            if lock_owned_by(obj, me) {
                return;
            }
        }
    }
}

/// C# `Monitor.Pulse(obj)`: move ONE `Monitor.Wait`-er (lowest id) into the acquire set; it is
/// handed the lock -- and woken -- by a later release, not now. Owner-only (loud otherwise).
#[no_mangle]
pub extern "C" fn lamella_monitor_pulse(obj: u32) {
    unsafe {
        sched_ensure_init();
        monitor_pulse_impl(obj, true);
    }
}

/// C# `Monitor.PulseAll(obj)`: move EVERY `Monitor.Wait`-er into the acquire set; each is handed
/// the lock by successive releases. Owner-only (loud otherwise).
#[no_mangle]
pub extern "C" fn lamella_monitor_pulse_all(obj: u32) {
    unsafe {
        sched_ensure_init();
        monitor_pulse_impl(obj, false);
    }
}

/// The shared Pulse/PulseAll body (the interp's `lock_pulse`): ownership-checked, then move the
/// lowest-id / every wait-set thread into `waiters`. Their saved grant depths ride
/// `LockTable::grant_depth` untouched.
unsafe fn monitor_pulse_impl(obj: u32, one: bool) {
    let me = (*SCHED).current;
    let t = &mut *LOCKS;
    let Some(i) = t.entries.iter().position(|e| e.obj == obj) else {
        monitor_not_owner_trap();
    };
    if t.entries[i].recursion == 0 || t.entries[i].owner != me {
        monitor_not_owner_trap();
    }
    let ws = t.entries[i].wait_set;
    if one {
        if ws != 0 {
            let w = ws.trailing_zeros();
            t.entries[i].wait_set &= !(1u32 << w);
            t.entries[i].waiters |= 1u32 << w;
        }
    } else {
        t.entries[i].wait_set = 0;
        t.entries[i].waiters |= ws;
    }
}


core::arch::global_asm!(
    ".syntax unified",
    ".thumb",
    ".weak __lamella_stackmaps_start",
    ".weak __lamella_stackmaps_end",
    ".align 2",
    "__lamella_stackmaps_start:",
    ".word 0",
    "__lamella_stackmaps_end:",
);

extern "C" {
    /// The pointer table's count word (see the weak fallback above).
    static __lamella_stackmaps_start: u32;
}

/// One `.lamella_stackmaps` record header -- the backend's `encode_stackmap_record` layout,
/// little-endian: `[func_addr u32][code_size u32][mode u16][frame_words u16][ret_lr_word u16]`
/// `[root_count u16]`, then `root_count` u16 root entries (bits 13:0 = word offset from the
/// stopped SP -- or from the region base for a mode-2 record -- bits 15:14 = kind).
#[repr(C)]
struct StackMapRecord {
    func_addr: u32,
    code_size: u32,
    mode: u16,
    frame_words: u16,
    ret_lr_word: u16,
    root_count: u16,
}

/// Record modes (mirrors `lamella_aot::arm32::STACKMAP_MODE_*`).
const STACKMAP_MODE_METHOD_SLOTS: u16 = 1;
const STACKMAP_MODE_STATICS: u16 = 2;

/// The gathered table as (first record pointer word, record count).
unsafe fn stackmap_table() -> (*const u32, usize) {
    let table = core::ptr::addr_of!(__lamella_stackmaps_start);
    (table.add(1), core::ptr::read(table) as usize)
}

/// The METHOD_SLOTS record covering `pc` (Thumb bit masked), or None -- a PC no record covers is a
/// Rust trampoline / boot stub / thread entry, which is exactly where a walk terminates.
unsafe fn stackmap_record_for(pc: u32) -> Option<*const StackMapRecord> {
    let (records, count) = stackmap_table();
    let pc = pc & !1;
    for i in 0..count {
        let record = core::ptr::read(records.add(i)) as *const StackMapRecord;
        if (*record).mode != STACKMAP_MODE_METHOD_SLOTS {
            continue;
        }
        let start = (*record).func_addr & !1;
        if pc >= start && pc < start.wrapping_add((*record).code_size) {
            return Some(record);
        }
    }
    None
}

/// The collector's per-root callback: `(slot address, kind)` with kind = 0 ObjectRef / 1 ManagedPtr
/// / 2 Pinned / 3 tagged (the record encoding's two kind bits). Null = count-only.
type RootVisitor = Option<extern "C" fn(*mut u32, u32)>;

/// Enumerate one root slot: report it, and count it live when its current content is nonzero (a
/// zero-initialized never-written slot enumerates without counting -- mirroring what a collector
/// would actually trace).
unsafe fn visit_root(slot: *mut u32, kind: u32, visit: RootVisitor, live: &mut u32) {
    if let Some(f) = visit {
        f(slot, kind);
    }
    if core::ptr::read_volatile(slot) != 0 {
        *live += 1;
    }
}

/// Walks ONE thread's managed frames from its anchor: enumerate the covering record's root slots
/// against the frame SP, then hop -- `next_pc = *(sp + ret_lr_word*4)`, `next_sp = sp +
/// frame_words*4` -- until a PC no record covers ends the walk. Every mode-1 record's
/// `frame_words >= 1` (a safepoint-bearing function always pushes LR), so SP strictly increases
/// and the walk terminates by construction.
unsafe fn walk_thread_stack(mut sp: u32, mut pc: u32, visit: RootVisitor, live: &mut u32) {
    while let Some(record) = stackmap_record_for(pc) {
        let roots =
            (record as *const u8).add(core::mem::size_of::<StackMapRecord>()) as *const u16;
        for i in 0..(*record).root_count as usize {
            let entry = core::ptr::read(roots.add(i));
            let slot = sp + u32::from(entry & 0x3FFF) * 4;
            visit_root(slot as *mut u32, u32::from(entry >> 14), visit, live);
        }
        pc = core::ptr::read((sp + u32::from((*record).ret_lr_word) * 4) as *const u32);
        sp += u32::from((*record).frame_words) * 4;
    }
}

/// The root walk: every live thread's managed stack from its anchor, then the global roots -- the
/// spawned-unfinished threads' entry delegates (`SCHED.entry_args`), the lock table's object keys
/// (the Tier 3 pin/relocate contract: Monitor keys on object ADDRESSES, so a relocator must be able
/// to rewrite them), and every mode-2 STATICS record's rows (the program's ref-bearing statics plus
/// the EH in-flight word at row 0). Returns the count of NONZERO enumerated slots. The calling
/// thread's own anchor was written by this export's entry shim, so its frames walk like any parked
/// thread's.
#[no_mangle]
extern "C" fn lamella_gc_walk_roots_impl(visit: RootVisitor) -> u32 {
    unsafe {
        let mut live = 0u32;
        let s = &mut *SCHED;
        let thread_count = (s.count as usize).clamp(1, MAX_THREADS);
        for i in 0..thread_count {
            if s.done & (1u32 << i) != 0 || s.anchor_pc[i] == 0 {
                continue;
            }
            walk_thread_stack(s.anchor_sp[i], s.anchor_pc[i], visit, &mut live);
        }
        for i in 1..thread_count {
            if s.done & (1u32 << i) == 0 {
                visit_root(core::ptr::addr_of_mut!(s.entry_args[i]), 0, visit, &mut live);
            }
        }
        let table = &mut *LOCKS;
        for entry in table.entries.iter_mut() {
            if entry.obj != 0 {
                visit_root(core::ptr::addr_of_mut!(entry.obj), 0, visit, &mut live);
            }
        }
        let (records, count) = stackmap_table();
        for i in 0..count {
            let record = core::ptr::read(records.add(i)) as *const StackMapRecord;
            if (*record).mode != STACKMAP_MODE_STATICS {
                continue;
            }
            let base = (*record).func_addr;
            let roots =
                (record as *const u8).add(core::mem::size_of::<StackMapRecord>()) as *const u16;
            for r in 0..(*record).root_count as usize {
                let entry = core::ptr::read(roots.add(r));
                let slot = base + u32::from(entry & 0x3FFF) * 4;
                visit_root(slot as *mut u32, u32::from(entry >> 14), visit, &mut live);
            }
        }
        live
    }
}

/// Count-only root walk (the e2e probe, and a cheap liveness sanity for the collector).
#[no_mangle]
extern "C" fn lamella_gc_count_roots_impl() -> u32 {
    lamella_gc_walk_roots_impl(None)
}

/// DEV PROBE: read a RAM/flash word from managed code (`[DllImport("lamella")]`) -- the
/// memory-inspection tool a QEMU fatal-lockup dump cannot provide (QEMU exits before a monitor
/// can look). Bisecting a heap/statics stomp from C# needs exactly one primitive: peek.
#[no_mangle]
extern "C" fn lamella_debug_peek(addr: u32) -> u32 {
    unsafe { core::ptr::read_volatile(addr as *const u32) }
}

/// DEV PROBE: print `value` as 8 hex digits + newline over semihosting -- [`lamella_debug_peek`]'s
/// output half, so a C# bisect probe can show a raw word on the QEMU console.
#[no_mangle]
extern "C" fn lamella_debug_print_hex(value: u32) {
    for shift in (0..8).rev() {
        let nibble = ((value >> (shift * 4)) & 0xF) as u8;
        console_put(if nibble < 10 { b'0' + nibble } else { b'a' + nibble - 10 });
    }
    console_put(b'\n');
}
