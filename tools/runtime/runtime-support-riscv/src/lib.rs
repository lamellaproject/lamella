//! Soft-float support for the RISC-V AOT linked image, cross-compiled for riscv32im and linked in on
//! demand (lamella-link's archive path pulls only the members a program reaches). The FPU-less RV32IM cores
//! lower every `double`/`float` op to a `compiler_builtins` helper; this crate references each one by its C
//! name in [`__lamella_force_softfloat_riscv`] (never called) so the archive carries the unmangled symbol
//! for the linker to pull.
#![no_std]
#![allow(
    clippy::not_unsafe_ptr_arg_deref,
    clippy::while_immutable_condition,
    clippy::empty_loop,
    clippy::neg_cmp_op_on_partial_ord
)]

use core::hint::black_box;
use core::panic::PanicInfo;

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    loop {}
}

extern "C" {
    fn __adddf3(a: f64, b: f64) -> f64;
    fn __subdf3(a: f64, b: f64) -> f64;
    fn __muldf3(a: f64, b: f64) -> f64;
    fn __divdf3(a: f64, b: f64) -> f64;
    fn __addsf3(a: f32, b: f32) -> f32;
    fn __subsf3(a: f32, b: f32) -> f32;
    fn __mulsf3(a: f32, b: f32) -> f32;
    fn __divsf3(a: f32, b: f32) -> f32;
    fn __eqdf2(a: f64, b: f64) -> i32;
    fn __nedf2(a: f64, b: f64) -> i32;
    fn __ltdf2(a: f64, b: f64) -> i32;
    fn __ledf2(a: f64, b: f64) -> i32;
    fn __gtdf2(a: f64, b: f64) -> i32;
    fn __gedf2(a: f64, b: f64) -> i32;
    fn __eqsf2(a: f32, b: f32) -> i32;
    fn __nesf2(a: f32, b: f32) -> i32;
    fn __ltsf2(a: f32, b: f32) -> i32;
    fn __lesf2(a: f32, b: f32) -> i32;
    fn __gtsf2(a: f32, b: f32) -> i32;
    fn __gesf2(a: f32, b: f32) -> i32;
    fn __floatsidf(i: i32) -> f64;
    fn __floatunsidf(u: u32) -> f64;
    fn __floatdidf(l: i64) -> f64;
    fn __floatundidf(w: u64) -> f64;
    fn __floatsisf(i: i32) -> f32;
    fn __floatdisf(l: i64) -> f32;
    fn __fixdfsi(a: f64) -> i32;
    fn __fixunsdfsi(a: f64) -> u32;
    fn __fixdfdi(a: f64) -> i64;
    fn __fixunsdfdi(a: f64) -> u64;
    fn __fixsfsi(a: f32) -> i32;
    fn __fixsfdi(a: f32) -> i64;
    fn __extendsfdf2(a: f32) -> f64;
    fn __truncdfsf2(a: f64) -> f32;
}

/// Never called -- it exists only to force each soft-float helper's unmangled C symbol into this archive, so
/// lamella-link can pull whichever a given program reaches. `black_box` on every argument and on the running
/// sum blocks LTO from folding an op away, so every referenced symbol stays a real archive member.
#[no_mangle]
pub extern "C" fn __lamella_force_softfloat_riscv() -> i64 {
    unsafe {
        let a = black_box(3.0f64);
        let b = black_box(7.0f64);
        let c = black_box(3.0f32);
        let d = black_box(7.0f32);
        let i = black_box(3i32);
        let u = black_box(7u32);
        let l = black_box(3i64);
        let w = black_box(7u64);

        let mut acc = 0i64;
        acc += black_box(__adddf3(a, b)) as i64;
        acc += black_box(__subdf3(a, b)) as i64;
        acc += black_box(__muldf3(a, b)) as i64;
        acc += black_box(__divdf3(a, b)) as i64;
        acc += black_box(__addsf3(c, d)) as i64;
        acc += black_box(__subsf3(c, d)) as i64;
        acc += black_box(__mulsf3(c, d)) as i64;
        acc += black_box(__divsf3(c, d)) as i64;

        acc += black_box(__eqdf2(a, b)) as i64;
        acc += black_box(__nedf2(a, b)) as i64;
        acc += black_box(__ltdf2(a, b)) as i64;
        acc += black_box(__ledf2(a, b)) as i64;
        acc += black_box(__gtdf2(a, b)) as i64;
        acc += black_box(__gedf2(a, b)) as i64;
        acc += black_box(__eqsf2(c, d)) as i64;
        acc += black_box(__nesf2(c, d)) as i64;
        acc += black_box(__ltsf2(c, d)) as i64;
        acc += black_box(__lesf2(c, d)) as i64;
        acc += black_box(__gtsf2(c, d)) as i64;
        acc += black_box(__gesf2(c, d)) as i64;

        acc += black_box(__floatsidf(i)) as i64;
        acc += black_box(__floatunsidf(u)) as i64;
        acc += black_box(__floatdidf(l)) as i64;
        acc += black_box(__floatundidf(w)) as i64;
        acc += black_box(__floatsisf(i)) as i64;
        acc += black_box(__floatdisf(l)) as i64;

        acc += black_box(__fixdfsi(a)) as i64;
        acc += black_box(__fixunsdfsi(a)) as i64;
        acc += black_box(__fixdfdi(a));
        acc += black_box(__fixunsdfdi(a)) as i64;
        acc += black_box(__fixsfsi(c)) as i64;
        acc += black_box(__fixsfdi(c));

        acc += black_box(__extendsfdf2(c)) as i64;
        acc += black_box(__truncdfsf2(a)) as i64;
        acc
    }
}


/// A saved RISC-V thread context: s0-s11 (callee-saved), then sp, then ra (the resume address). Laid out
/// to match [`lamella_thread_switch_riscv`]'s stores: `saved[0..12]` = s0..s11 at 0..48, `sp` at 48, `ra`
/// at 52. A fresh thread is built with `sp` = its stack top and `ra` = its entry.
#[repr(C)]
struct ThreadContext {
    saved: [u32; 12],
    sp: u32,
    ra: u32,
}

core::arch::global_asm!(
    ".global lamella_thread_switch_riscv",
    "lamella_thread_switch_riscv:",
    "sw   s0,   0(a0)",
    "sw   s1,   4(a0)",
    "sw   s2,   8(a0)",
    "sw   s3,  12(a0)",
    "sw   s4,  16(a0)",
    "sw   s5,  20(a0)",
    "sw   s6,  24(a0)",
    "sw   s7,  28(a0)",
    "sw   s8,  32(a0)",
    "sw   s9,  36(a0)",
    "sw   s10, 40(a0)",
    "sw   s11, 44(a0)",
    "sw   sp,  48(a0)",
    "sw   ra,  52(a0)",
    "lw   s0,   0(a1)",
    "lw   s1,   4(a1)",
    "lw   s2,   8(a1)",
    "lw   s3,  12(a1)",
    "lw   s4,  16(a1)",
    "lw   s5,  20(a1)",
    "lw   s6,  24(a1)",
    "lw   s7,  28(a1)",
    "lw   s8,  32(a1)",
    "lw   s9,  36(a1)",
    "lw   s10, 40(a1)",
    "lw   s11, 44(a1)",
    "lw   sp,  48(a1)",
    "lw   ra,  52(a1)",
    "ret",
);

extern "C" {
    fn lamella_thread_switch_riscv(save: *mut ThreadContext, restore: *const ThreadContext);
}

const CTX_MAIN: *mut ThreadContext = 0x8040_0000 as *mut ThreadContext;
const CTX_WORKER: *mut ThreadContext = 0x8040_0040 as *mut ThreadContext;
const PING_COUNT: *mut u32 = 0x8040_0080 as *mut u32;
const WORKER_STACK_TOP: u32 = 0x8040_1000;

/// The worker green thread: bump the shared counter, then yield back to main. Never returns.
extern "C" fn worker_entry_riscv() -> ! {
    loop {
        unsafe {
            let n = core::ptr::read_volatile(PING_COUNT);
            core::ptr::write_volatile(PING_COUNT, n.wrapping_add(1));
            lamella_thread_switch_riscv(CTX_WORKER, CTX_MAIN);
        }
    }
}

/// The RISC-V twin of the thumb ping-pong proof: spawn one worker green thread and ping-pong 21 times.
/// `main` keeps a `sum` LIVE across each switch (testing the caller-side {s0-s11} round-trip); the worker
/// bumps a counter on its OWN stack (testing sp/ra). Returns `sum + count` = 21 + 21 = **42** iff every
/// switch round-tripped both register sets and both stacks.
#[no_mangle]
pub extern "C" fn lamella_thread_pingpong_demo_riscv() -> u32 {
    unsafe {
        core::ptr::write_volatile(PING_COUNT, 0);
        core::ptr::write(
            CTX_WORKER,
            ThreadContext {
                saved: [0; 12],
                sp: WORKER_STACK_TOP & !15,
                ra: worker_entry_riscv as *const () as usize as u32,
            },
        );
        let mut sum = 0u32;
        for _ in 0..21 {
            sum = sum.wrapping_add(1);
            lamella_thread_switch_riscv(CTX_MAIN, CTX_WORKER);
        }
        sum.wrapping_add(core::ptr::read_volatile(PING_COUNT))
    }
}


const MAX_THREADS: usize = 4;
/// Thread `id`'s stack top (4 KiB apart). Thread 0 (main) keeps the boot stack; only ids 1.. use these.
const fn worker_stack_top(id: usize) -> u32 {
    0x8040_0000 + (id as u32) * 0x1000
}

/// Why a parked thread is waiting, for the reactor block point below. Mirrors
/// `lamella_cil_runtime::reactor::WaitReason` plus a `NotParked` slot (so the per-thread array needs no
/// `Option`). A `Join`/lock park is woken by hand-off, not the reactor, so it stays `NotParked` here.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ParkReason {
    /// Runnable or lock/join-blocked -- not on the reactor's timer/socket set.
    NotParked,
    /// `Thread.Sleep` until this monotonic-millisecond deadline.
    Sleep(u64),
    /// A socket op returned WouldBlock; parked until this handle is ready.
    Io(u32),
}

/// LAYOUT CONTRACT (the anchor shims read these by offset; the ARM crate's twin): every field up
/// to and including `anchor_pc` is `repr(C)`-deterministic -- `contexts` = 4 x 56 bytes, so
/// `count` is at +224, `current` at +228, `anchor_sp[0]` at +236, `anchor_pc[0]` at +252 (the two
/// arrays sit 16 bytes apart). `parks` (a Rust enum, unspecified layout) and later fields are
/// Rust-only.
#[repr(C)]
struct Scheduler {
    contexts: [ThreadContext; MAX_THREADS],
    count: u32,
    current: u32,
    runnable: u32,
    /// The managed-frame ANCHOR the GC stack walk starts from (design sec 2.3 refinement 1; the
    /// ARM crate documents the full contract): SP at the thread's last managed->runtime seam call,
    /// written by the seam entry shims below. RISC-V records and its walker are not built here yet;
    /// the anchors are written regardless, so the Scheduler stays the ARM struct's twin.
    anchor_sp: [u32; MAX_THREADS],
    /// The anchor's PC twin (the seam call's `ra`); 0 = never crossed a seam.
    anchor_pc: [u32; MAX_THREADS],
    parks: [ParkReason; MAX_THREADS],
    /// A C#-spawned thread's compiled managed entry helper (`Thread.ThreadEntry`'s code address) ...
    entries: [u32; MAX_THREADS],
    /// ... and the ThreadStart delegate it invokes (an object pointer, opaque to this side).
    entry_args: [u32; MAX_THREADS],
    /// Bit `i` set = thread `i` has exited -- what `lamella_thread_join` waits on.
    done: u32,
}
const SCHED: *mut Scheduler = 0x8040_4000 as *mut Scheduler;
const SCHED_COUNTER: *mut u32 = 0x8040_4800 as *mut u32;

macro_rules! anchor_seam_shim {
    ($name:literal) => {
        core::arch::global_asm!(
            concat!(".global ", $name),
            concat!($name, ":"),
            "li   t0, 0x804040E4",
            "lw   t1, 0(t0)",
            "slli t1, t1, 2",
            "li   t0, 0x804040EC",
            "add  t0, t0, t1",
            "sw   sp, 0(t0)",
            "sw   ra, 16(t0)",
            concat!("tail ", $name, "_impl"),
        );
    };
}

anchor_seam_shim!("lamella_thread_yield");
anchor_seam_shim!("lamella_thread_join");
anchor_seam_shim!("lamella_thread_sleep");
anchor_seam_shim!("lamella_monitor_enter");
anchor_seam_shim!("lamella_monitor_wait");
anchor_seam_shim!("lamella_gc_walk_roots");
anchor_seam_shim!("lamella_gc_count_roots");

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

unsafe fn sched_yield() {
    let s = &mut *SCHED;
    let from = s.current as usize;
    let next = sched_next_runnable(s, from);
    if next == from {
        return;
    }
    s.current = next as u32;
    lamella_thread_switch_riscv(&mut s.contexts[from], &s.contexts[next]);
}

/// End the current thread; with nothing else runnable the exiting thread drives the reactor block
/// point until a parked thread wakes (switching to a stale saved context instead would re-run dead
/// code). Never returns -- a cleared thread is never scheduled again.
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
                saved: [0; 12],
                sp: 0,
                ra: 0,
            };
            lamella_thread_switch_riscv(&mut dead, &s.contexts[next]);
            loop {}
        }
        if !sched_block_point(s) {
            sched_deadlock_trap();
        }
    }
}

unsafe fn sched_spawn(entry: extern "C" fn() -> !) {
    let s = &mut *SCHED;
    let id = s.count as usize;
    core::ptr::write(
        &mut s.contexts[id],
        ThreadContext {
            saved: [0; 12],
            sp: worker_stack_top(id) & !15,
            ra: entry as *const () as usize as u32,
        },
    );
    s.anchor_sp[id] = 0;
    s.anchor_pc[id] = 0;
    s.runnable |= 1u32 << id;
    s.count += 1;
}

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

/// The RV32 twin of the thumb scheduler proof: spawn THREE worker green threads, round-robin until all
/// exit; each bumps the shared counter 7x by 2 on its own stack, so the total is 3*7*2 = **42** iff spawn,
/// the round-robin ready queue, per-thread stacks + {s0-s11,sp,ra} state across yields, and exit all work.
#[no_mangle]
pub extern "C" fn lamella_sched_demo_riscv() -> u32 {
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


/// Mock monotonic clock (ms) for the self-contained reactor demo; a net build reads the real clock seam.
const REACTOR_NOW: *mut u64 = 0x8040_4C00 as *mut u64;
/// The one socket handle the mock poll reports ready (0 = none); a net build calls the real network poll.
const REACTOR_NET_READY: *mut u32 = 0x8040_4C08 as *mut u32;

/// The mock reactor environment over [`REACTOR_NOW`]/[`REACTOR_NET_READY`] -- no clock hardware, no
/// net stack. The mock "wait" ADVANCES the clock by the timeout (there is no real time source), so a
/// timed park deterministically wakes. The RV32 twin of the ARM crate's featureless flavor.
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
fn sched_any_parked(s: &Scheduler) -> bool {
    s.parks[..s.count as usize].iter().any(|p| *p != ParkReason::NotParked)
}

/// A parked/exiting/joining thread found NOTHING runnable and the block point NOTHING external to wait
/// on: every live thread is blocked on a join/lock nobody can satisfy. Fail LOUD -- write the QEMU
/// `virt` SiFive test-finisher FAIL with exit code 2 (the finisher exits with the HIGH sixteen bits,
/// so a bare 0x3333 would exit 0, indistinguishable from PASS; real silicon has no finisher and halts
/// in the loop) rather than silently spinning forever. The ARM twin prints `DEADLOCK`; the trap codes
/// here are DEADLOCK = 2, LOCKFULL = 3, MONITOR = 4, NULLLOCK = 5 (harness wrong-result FAILs use 1).
fn sched_deadlock_trap() -> ! {
    const FINISHER: *mut u32 = 0x0010_0000 as *mut u32;
    unsafe { core::ptr::write_volatile(FINISHER, 0x0002_3333) };
    loop {}
}

/// The ONE byte-oriented console sink -- the RISC-V implementation of the same symbol the ARM crate
/// exports. The AOT's `Debug.WriteLine` / `Console.WriteLine(int)` intrinsics format front-end-side
/// and call this with `(ptr, len)`, so adding a target (or a new RISC-V ISA profile) means writing
/// this ONE function over that board's channel and changing nothing in the compiler.
///
/// This crate targets QEMU's `virt` board throughout (its RAM addresses and the SiFive test finisher
/// above are all virt's), so the channel here is virt's 16550 UART: a byte stored to the transmit
/// holding register at 0x1000_0000 goes to QEMU's console. No line-status poll -- virt's model
/// accepts a write immediately; a REAL 16550 would spin on LSR bit 5 (THR empty) first, which is
/// exactly the kind of per-board difference this seam exists to absorb.
#[no_mangle]
pub extern "C" fn lamella_console_write_bytes(ptr: *const u8, len: usize) {
    for i in 0..len {
        console_put(unsafe { *ptr.add(i) });
    }
}


/// The ONE place a console byte reaches this target's channel: QEMU `virt`'s 16550 UART transmit
/// holding register. Every writer below funnels through here, so pointing this crate at a different
/// destination is one edit. `#[inline(always)]` for the same reason the ARM twin is: a size-tuned LTO
/// would otherwise leave an internal symbol the archive link never pulls (it pulls the exported
/// member, not each internal callee), leaving the image undefined.
///
/// No line-status poll -- virt's model accepts a write immediately; a REAL 16550 would spin on LSR
/// bit 5 (THR empty) first, which is the kind of per-board difference this seam exists to absorb.
#[inline(always)]
fn console_put(byte: u8) {
    const UART_THR: *mut u8 = 0x1000_0000 as *mut u8;
    unsafe { core::ptr::write_volatile(UART_THR, byte) };
}

/// Emit one UTF-16 code unit UTF-8-encoded (BMP; a lone surrogate is emitted per-unit), as the
/// interpreter writes a string's code units to its console.
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

/// Powers of ten from 10^19 down to 1 (10^19 < `u64::MAX`), so the decimal writer can subtract rather
/// than divide.
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

/// Write an unsigned decimal, most-significant digit first, by repeated SUBTRACTION of powers of ten.
/// Leading zeros are suppressed; `0` prints `0`.
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

/// `Console.Write(string)`: `s` is the ObjectRef an `ldstr` produces -- a pointer at the AOT string
/// layout `[len: u32][u16 code units ...]`. A null string writes nothing.
///
/// A DIFFERENT ABI from [`lamella_console_write_bytes`] above (utf16 units against raw bytes), which
/// is why the two cannot share a symbol however similar they look.
#[no_mangle]
pub extern "C" fn lamella_console_write(s: *const u32) {
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

#[no_mangle]
pub extern "C" fn lamella_console_write_i32(v: i32) {
    write_idec(i64::from(v));
}

#[no_mangle]
pub extern "C" fn lamella_console_write_u32(v: u32) {
    write_udec(u64::from(v));
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
            lamella_thread_switch_riscv(&mut s.contexts[cur], &s.contexts[next]);
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
unsafe fn reactor_worker(reason: ParkReason) -> ! {
    let c = core::ptr::read_volatile(SCHED_COUNTER);
    core::ptr::write_volatile(SCHED_COUNTER, c.wrapping_add(14));
    sched_park(reason);
    let c = core::ptr::read_volatile(SCHED_COUNTER);
    core::ptr::write_volatile(SCHED_COUNTER, c.wrapping_add(7));
    sched_exit();
}

extern "C" fn reactor_worker_io() -> ! {
    unsafe { reactor_worker(ParkReason::Io(7)) }
}
extern "C" fn reactor_worker_sleep() -> ! {
    unsafe { reactor_worker(ParkReason::Sleep(50)) }
}

/// The RV32 twin of the thumb reactor proof: spawn two workers -- one parks on a SOCKET (Io), one on a TIMER
/// (Sleep) -- so `runnable` drops to just `main` with both parked; `main` then reaches the block point, which
/// wakes BOTH in one wait. Each worker bumps the counter 14 before parking and 7 after waking, so the total
/// is 2*(14+7) = **42** iff park, the block point, and the resume-after-wake all work.
#[no_mangle]
pub extern "C" fn lamella_reactor_demo_riscv() -> u32 {
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
        let entry: extern "C" fn(u32) = core::mem::transmute((*SCHED).entries[cur] as usize);
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

/// C# `Thread.Sleep(ms)`: no clock seam is linked on this target yet (no RISC-V net stack), so a
/// timed park could never truly wait; degrade to one cooperative yield (the managed surface's
/// documented no-clock behavior). When a RISC-V net archive lands, mirror the ARM crate's `net`
/// flavor: park `Sleep(lamella_net_now_ms() + ms)` on the reactor.
#[no_mangle]
extern "C" fn lamella_thread_sleep_impl(_milliseconds: i32) {
    lamella_thread_yield_impl();
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
/// 0x8040_4810..0x8040_48C0, clear of the mock reactor cells (0x8040_4C00+) and the worker stacks
/// (0x8040_0000..0x8040_3000). QEMU zeroes RAM (= every slot free); a silicon boot zeroes it
/// alongside SCHED.
const LOCKS: *mut LockTable = 0x8040_4810 as *mut LockTable;

/// More than [`MAX_LOCKS`] DISTINCT objects locked/awaited at once: the fixed table cannot grow,
/// and proceeding without a slot would drop mutual exclusion. Writes the virt test-finisher FAIL
/// with exit code 3 -- the trap-code map at [`sched_deadlock_trap`] (the ARM twin prints
/// `LOCKFULL`; this target has no console in the e2e harnesses).
fn monitor_table_full_trap() -> ! {
    const FINISHER: *mut u32 = 0x0010_0000 as *mut u32;
    unsafe { core::ptr::write_volatile(FINISHER, 0x0003_3333) };
    loop {}
}

/// `Monitor.Wait`/`Pulse`/`PulseAll` by a thread that does not own the lock -- the interpreter's
/// `SynchronizationLockException` site. A native seam cannot raise a managed exception, so fail
/// LOUD (finisher FAIL with exit code 4 -- the trap-code map at [`sched_deadlock_trap`]; the ARM
/// twin prints `MONITOR`) rather than silently diverge. (A non-owner `Exit` is different: the
/// interp releases nothing and stays silent -- mirrored.)
fn monitor_not_owner_trap() -> ! {
    const FINISHER: *mut u32 = 0x0010_0000 as *mut u32;
    unsafe { core::ptr::write_volatile(FINISHER, 0x0004_3333) };
    loop {}
}

/// `Monitor.Enter(null)`/`TryEnter(null)`: the interp traps LOUD (`Trap::TypeMismatch` on a
/// non-Object), but this table's key 0 doubles as the free-slot sentinel, so accepting it risks a
/// held/awaited null lock being clobbered by a later new-slot scan. Finisher FAIL exit 5 (the
/// trap-code map at [`sched_deadlock_trap`]; the ARM twin prints `NULLLOCK`).
fn monitor_null_trap() -> ! {
    const FINISHER: *mut u32 = 0x0010_0000 as *mut u32;
    unsafe { core::ptr::write_volatile(FINISHER, 0x0005_3333) };
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

/// One `.lamella_stackmaps` record header -- the backend's shared `encode_stackmap_record`
/// layout, little-endian: `[func_addr u32][code_size u32][mode u16][frame_words u16]`
/// `[ret_ra_word u16][root_count u16]`, then `root_count` u16 root entries (bits 13:0 = word
/// offset from the stopped SP -- or from the region base for a mode-2 record -- bits 15:14 =
/// kind). The field the ARM walker calls `ret_lr_word` holds the saved-`ra` slot here; the wire
/// format is identical.
#[repr(C)]
struct StackMapRecord {
    func_addr: u32,
    code_size: u32,
    mode: u16,
    frame_words: u16,
    ret_ra_word: u16,
    root_count: u16,
}

/// Record modes (mirrors `lamella_aot`'s shared `stackmaps` constants).
const STACKMAP_MODE_METHOD_SLOTS: u16 = 1;
const STACKMAP_MODE_STATICS: u16 = 2;

/// The gathered table as (first record pointer word, record count).
unsafe fn stackmap_table() -> (*const u32, usize) {
    let table = core::ptr::addr_of!(__lamella_stackmaps_start);
    (table.add(1), core::ptr::read(table) as usize)
}

/// The METHOD_SLOTS record covering `pc`, or None -- a PC no record covers is a Rust trampoline /
/// boot stub / thread entry, which is exactly where a walk terminates. (No Thumb-bit masking
/// here: a RISC-V code address has no mode bit.)
unsafe fn stackmap_record_for(pc: u32) -> Option<*const StackMapRecord> {
    let (records, count) = stackmap_table();
    for i in 0..count {
        let record = core::ptr::read(records.add(i)) as *const StackMapRecord;
        if (*record).mode != STACKMAP_MODE_METHOD_SLOTS {
            continue;
        }
        let start = (*record).func_addr;
        if pc >= start && pc < start.wrapping_add((*record).code_size) {
            return Some(record);
        }
    }
    None
}

/// The collector's per-root callback: `(slot address, kind)` with kind = 0 ObjectRef / 1
/// ManagedPtr / 2 Pinned / 3 tagged (the record encoding's two kind bits). Null = count-only.
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
/// against the frame SP, then hop -- `next_pc = *(sp + ret_ra_word*4)`, `next_sp = sp +
/// frame_words*4` -- until a PC no record covers ends the walk. Every mode-1 record's
/// `frame_words >= 1` (a safepoint-bearing function always saves `ra` in a nonempty frame), so SP
/// strictly increases and the walk terminates by construction.
unsafe fn walk_thread_stack(mut sp: u32, mut pc: u32, visit: RootVisitor, live: &mut u32) {
    while let Some(record) = stackmap_record_for(pc) {
        let roots =
            (record as *const u8).add(core::mem::size_of::<StackMapRecord>()) as *const u16;
        for i in 0..(*record).root_count as usize {
            let entry = core::ptr::read(roots.add(i));
            let slot = sp + u32::from(entry & 0x3FFF) * 4;
            visit_root(slot as *mut u32, u32::from(entry >> 14), visit, live);
        }
        pc = core::ptr::read((sp + u32::from((*record).ret_ra_word) * 4) as *const u32);
        sp += u32::from((*record).frame_words) * 4;
    }
}

/// The root walk: every live thread's managed stack from its anchor, then the global roots -- the
/// spawned-unfinished threads' entry delegates (`SCHED.entry_args`), the lock table's object keys
/// (the pin/relocate contract: Monitor keys on object ADDRESSES, so a relocator must be able to
/// rewrite them), and every mode-2 STATICS record's rows (each assembly's ref-bearing statics
/// plus the program's EH in-flight word at row 0). Returns the count of NONZERO enumerated slots.
/// The calling thread's own anchor was written by this export's entry shim, so its frames walk
/// like any parked thread's.
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


/// The bump allocator's high-water cursor (the harness RAM plan: cursor word at 0x8010_0000, heap
/// growing from 0x8010_0004) -- the same word every example's `lamella_gc_alloc` provider bumps.
const HEAP_PTR: *mut u32 = 0x8010_0000 as *mut u32;

/// Bump-allocate `bytes` (rounded to 8, the provider's rounding), returning the block base.
fn bump_alloc(bytes: u32) -> *mut u32 {
    unsafe {
        let base = core::ptr::read_volatile(HEAP_PTR);
        core::ptr::write_volatile(HEAP_PTR, base + ((bytes + 7) & !7));
        base as *mut u32
    }
}

anchor_seam_shim!("lamella_string_substring");
anchor_seam_shim!("lamella_char_to_string");
anchor_seam_shim!("lamella_double_to_string");

/// `System.String.Substring(start, len)` on device: allocate a fresh `[len: u32][u16 units ...]`
/// string and copy `len` code units starting at index `start` from the source string `s`. No
/// bounds check -- an in-range call matches the interpreter exactly; the .NET
/// `ArgumentOutOfRangeException` for an out-of-range range is a program contract, not raised on
/// device. Backs `String.Substring` and, through it, `Trim`/`TrimStart`/`TrimEnd`/`Remove`.
#[no_mangle]
extern "C" fn lamella_string_substring_impl(s: *const u32, start: u32, len: u32) -> *mut u32 {
    let obj = bump_alloc(4 + 2 * len);
    unsafe {
        core::ptr::write(obj, len);
        let src = (s as *const u8).add(4) as *const u16;
        let dst = (obj as *mut u8).add(4) as *mut u16;
        for i in 0..len {
            core::ptr::write(
                dst.add(i as usize),
                core::ptr::read_volatile(src.add((start + i) as usize)),
            );
        }
    }
    obj
}

/// `System.Char.ToString()` on device: a one-unit string holding the code unit `c`.
#[no_mangle]
extern "C" fn lamella_char_to_string_impl(c: u32) -> *mut u32 {
    let obj = bump_alloc(6);
    unsafe {
        core::ptr::write(obj, 1);
        core::ptr::write((obj as *mut u8).add(4) as *mut u16, c as u16);
    }
    obj
}

/// The longest `Double.ToString` rendering (the thumb crate's sizing: scientific ~24 chars, fixed
/// ~24; 64 is ample margin -- a too-small buffer would truncate and break parity).
const F64_STR_CAP: usize = 64;

/// A `core::fmt::Write` sink into a byte slice; writes past the end are dropped (the caller sizes
/// the buffer).
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

/// Formats `value` into `buf` EXACTLY as the interpreter's `format_double` does (the thumb
/// crate's body, unchanged): `Infinity`/`-Infinity` spelled out; otherwise .NET's G notation --
/// scientific `<mantissa>E<+/-><exp>` iff the base-10 exponent is `<= -5` or `>= 17`, else the
/// plain `to_string()` rendering. Both forms come from `core::fmt` (which IS Rust's float
/// formatting, the interpreter's source of digits), so device text stays byte-identical by
/// construction. Returns the length.
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

/// `System.Double.ToString()` on device: format `value` (byte-identical to the interpreter) and
/// return a managed string -- the same `[len: u32][u16 code units ...]` shape every string reader
/// consumes (each ASCII digit widens to one code unit).
#[no_mangle]
extern "C" fn lamella_double_to_string_impl(value: f64) -> *mut u32 {
    let mut buf = [0u8; F64_STR_CAP];
    let len = format_f64(value, &mut buf);
    let obj = bump_alloc((4 + 2 * len) as u32);
    unsafe {
        core::ptr::write(obj, len as u32);
        let units = (obj as *mut u8).add(4) as *mut u16;
        for (i, &b) in buf[..len].iter().enumerate() {
            core::ptr::write(units.add(i), b as u16);
        }
    }
    obj
}


/// DEV PROBE: read a RAM/flash word from managed code -- the memory-inspection tool a QEMU fatal
/// lockup cannot provide (QEMU exits before a monitor can look). Bisecting a heap/statics stomp
/// from C# needs exactly one primitive: peek.
#[no_mangle]
extern "C" fn lamella_debug_peek(addr: u32) -> u32 {
    unsafe { core::ptr::read_volatile(addr as *const u32) }
}

/// DEV PROBE: print `value` as 8 hex digits + newline -- [`lamella_debug_peek`]'s output half, so a
/// C# bisect probe can show a raw word on the console. Routed through
/// [`lamella_console_write_bytes`] rather than poking the UART directly, so this probe reaches
/// whatever channel that seam names and a new board changes one function, not two.
#[no_mangle]
extern "C" fn lamella_debug_print_hex(value: u32) {
    let mut line = [0u8; 9];
    for shift in 0..8 {
        let nibble = ((value >> ((7 - shift) * 4)) & 0xF) as u8;
        line[shift] = if nibble < 10 { b'0' + nibble } else { b'a' + nibble - 10 };
    }
    line[8] = b'\n';
    lamella_console_write_bytes(line.as_ptr(), line.len());
}
