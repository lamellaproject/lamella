//! The AOT image's Lamella Link layer: the frame loop and codec, plus the on-target debug agent,
//! exposed as C-ABI entry points the image's boot and service paths call.

#![cfg_attr(not(test), no_std)]
#![allow(unsafe_code)]

extern crate alloc;


use lamella_wire::{Frame, FrameReader, Transport, TransportError, encode_frame};

const CURSOR_OFF: usize = 0;
const END_OFF: usize = 8;
const STATE_OFF: usize = 16;

/// The pinned RAM word holding the caller-provided region base (0 before init).
///
/// On the device it is a fixed low-RAM address adjacent to the networking layer's
/// (`0x2000_0104`) and `lamella_gc_alloc`'s `HEAP_PTR` (`0x2000_0100`). On the host a real `.bss`
/// word stands in, so the seam is exercised in a test without the device address.
#[cfg(target_os = "none")]
#[inline]
fn rt_base_slot() -> *mut usize {
    0x2000_0108 as *mut usize
}

#[cfg(not(target_os = "none"))]
#[inline]
fn rt_base_slot() -> *mut usize {
    static mut HOST_SLOT: usize = 0;
    core::ptr::addr_of_mut!(HOST_SLOT)
}

/// The layer's own mutable state, living in the caller's region.
///
/// The live window is one of those cells rather than a pair of statics in the agent, for the reason
/// the module note gives: in an image this linker produces, two such words resolve into `.text`, so
/// reading the window would read a code word and installing one would corrupt code.
struct RtState {
    reader: FrameReader,
    /// The board facts a `HELLO_ACK` advertises, from [`lamella_debug_rt_set_identity`]. Zero until
    /// boot supplies them, which reads as unknown throughout the identity.
    identity: BoardIdentity,
    #[cfg(feature = "debug-agent")]
    window: (u32, u32),
    /// The update region being filled, and the running checksum of what is in it.
    #[cfg(feature = "fw-update")]
    image: lamella_fw_store::image::ImageStore,
    /// Bytes of update region the board's flash seam covers, from
    /// [`lamella_debug_rt_set_fw_region`]. 0 until boot declares it, and `FW_UPDATE` stays clear.
    #[cfg(feature = "fw-update")]
    fw_region: u32,
    /// Bytes of LOCATOR region the board's locator seam covers, from
    /// [`lamella_debug_rt_set_locator_region`]. 0 until boot declares it, and `FW_ACTIVATE` is then
    /// refused BY NAME rather than answered -- an image whose boot never wired a locator genuinely
    /// does not implement activation, and saying so is the true answer rather than a slot refusal.
    #[cfg(feature = "fw-update")]
    locator_region: u32,
    /// Which firmware slot is RUNNING, from the same call. It is what resolves a host's
    /// [`lamella_wire::msg::fw_slot::OTHER`], and it is a boot fact: only the code that chose this
    /// image knows which side it booted from.
    #[cfg(feature = "fw-update")]
    running_slot: u8,
}

/// What the IMAGE knows about its own board and this archive cannot: the product code and the
/// chip's own identity registers.
///
/// The archive is built once per ISA and serves every board on it, so these are the fields that
/// arrive from outside -- exactly the split `lamella-runner`'s `set_board_identity` already makes
/// for the interpreted tier. `arch` is NOT here: it is a property of this build rather than of the
/// board, and the build script reads it.
#[derive(Clone, Copy, Default)]
struct BoardIdentity {
    product_model: u16,
    chip_idcode: u32,
    chip_devid: u32,
}

/// The caller region base, or 0 before [`lamella_debug_rt_init_ram`].
#[inline]
fn rt_base() -> usize {
    unsafe { *rt_base_slot() }
}

/// The frame-loop state in the caller region, or `None` before init.
fn rt_state() -> Option<&'static mut RtState> {
    let base = rt_base();
    if base == 0 {
        return None;
    }
    Some(unsafe { &mut *((base + STATE_OFF) as *mut RtState) })
}

/// Bump-allocate `layout` from the caller region -- RAW cursor/end words only, so it is safe to
/// call while the [`RtState`] cell is borrowed (which is exactly what dispatching a frame does).
/// Returns null before init or on exhaustion.
pub fn rt_alloc(layout: core::alloc::Layout) -> *mut u8 {
    let base = rt_base();
    if base == 0 {
        return core::ptr::null_mut();
    }
    unsafe {
        let cursor = (base + CURSOR_OFF) as *mut usize;
        let end = *((base + END_OFF) as *const usize);
        let align = layout.align().max(core::mem::size_of::<usize>());
        let start = (*cursor + align - 1) & !(align - 1);
        let stop = start + layout.size();
        if stop > end {
            return core::ptr::null_mut();
        }
        *cursor = stop;
        start as *mut u8
    }
}

/// The bump frontier, as a value [`rt_release_to`] can return the allocator to. 0 before init.
fn rt_mark() -> usize {
    let base = rt_base();
    if base == 0 {
        return 0;
    }
    unsafe { *((base + CURSOR_OFF) as *const usize) }
}

/// Return the bump frontier to `mark`, reclaiming every allocation made since it was taken.
///
/// # Why a bump allocator that never frees still needs this
///
/// The archive's global allocator bumps and its `dealloc` does nothing, so on a device every byte a
/// served frame allocates is consumed for the life of the image -- even though Rust dropped it
/// microseconds later. Measured on this crate's own service path, a `HELLO` exchange allocates 128
/// bytes and frees 128, and a `PING` allocates 16 and frees 16: in the steady state the totals are
/// EQUAL, so all of it is transient and all of it is recoverable. Without this, a 4 KiB region ends
/// after about thirty handshakes and an 8 KiB one after about sixty -- and a debugger attaches once
/// per attach, so that is a link that dies during an ordinary afternoon's work, silently, looking
/// exactly like a carrier fault.
///
/// # Safety
/// Nothing allocated after `mark` may still be referenced. The one caller takes `mark` AFTER the
/// frame reader has run, so everything the reader owns lies below it and is never reclaimed.
unsafe fn rt_release_to(mark: usize) {
    let base = rt_base();
    if base == 0 || mark == 0 {
        return;
    }
    unsafe { *((base + CURSOR_OFF) as *mut usize) = mark }
}

/// Installs the Link layer over a caller-provided RAM region `[base, base+len)`. The image's boot
/// calls this ONCE before any other entry point here.
///
/// A region too small to hold the control words and the state is REFUSED rather than trimmed: a
/// frame loop with no room for its reader would accept bytes and drop every frame, which looks
/// exactly like a dead carrier.
///
/// # Safety
/// `base` must be the start of at least `len` bytes of RAM this image owns for as long as it runs.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lamella_debug_rt_init_ram(base: usize, len: usize) -> i32 {
    let Some((base, state_end)) = checked_region(base, len) else { return -1 };
    unsafe {
        core::ptr::write(
            (base + STATE_OFF) as *mut RtState,
            RtState {
                reader: FrameReader::with_max_payload(lamella_wire::MIN_INBOUND_PAYLOAD),
                identity: BoardIdentity::default(),
                #[cfg(feature = "debug-agent")]
                window: (0, 0),
                #[cfg(feature = "fw-update")]
                image: lamella_fw_store::image::ImageStore::new(0),
                #[cfg(feature = "fw-update")]
                fw_region: 0,
                #[cfg(feature = "fw-update")]
                locator_region: 0,
                #[cfg(feature = "fw-update")]
                running_slot: 0,
            },
        );
        *((base + CURSOR_OFF) as *mut usize) = (base + state_end + 7) & !7;
        *((base + END_OFF) as *mut usize) = base + len;
        *rt_base_slot() = base;
    }
    0
}

/// An 8-aligned region big enough for the control words and the state, as `(base, state_end)`, or
/// `None`.
///
/// Split from [`lamella_debug_rt_init_ram`] so the rule is checkable without a test writing the
/// pinned word -- that word is process-wide, and a test that wrote it would decide the outcome of
/// every other test in the process depending on which ran first. Same reason the agent's own
/// `checked_window` exists.
fn checked_region(base: usize, len: usize) -> Option<(usize, usize)> {
    let aligned = (base + 7) & !7;
    let state_end = STATE_OFF + core::mem::size_of::<RtState>();
    let usable = len.checked_sub(aligned - base)?;
    (usable > state_end).then_some((aligned, state_end))
}

/// Declares the address span the debug agent may read and write, and by doing so turns it on.
/// Until this is called every live request is refused. The image's boot supplies its own RAM map.
/// Returns 0, or -1 before [`lamella_debug_rt_init_ram`] or for a span whose end does not exist --
/// a bad window is refused HERE rather than at request time, so it can never widen into an
/// unbounded one.
#[cfg(feature = "debug-agent")]
#[unsafe(no_mangle)]
pub extern "C" fn lamella_debug_rt_set_window(base: u32, len: u32) -> i32 {
    let Some(state) = rt_state() else { return -1 };
    let Some(window) = lamella_debug_agent::checked_window(base, len) else { return -1 };
    state.window = window;
    0
}

/// Installs the board facts this image advertises in its `HELLO_ACK`: the board-model code from
/// `lamella_wire::product_model` (0 for a custom board), the chip's debug-port identification code,
/// and the vendor device-id register the boot read from its own silicon. Each 0 means unknown.
/// Call once at boot, before serving. Returns 0, or -1 before [`lamella_debug_rt_init_ram`].
///
/// # Why the image supplies these and the archive cannot
///
/// They are BOARD facts and this archive is built once per ISA for every board -- the same division
/// as the carrier symbols. An image that never calls this advertises zeros, which read as unknown
/// rather than as a claim: the honest answer for a board whose boot has not been taught to read its
/// own identity registers.
///
/// # Safety
/// Callable from C at any time after init; it writes only the caller region's state cell.
#[unsafe(no_mangle)]
pub extern "C" fn lamella_debug_rt_set_identity(
    product_model: u16,
    chip_idcode: u32,
    chip_devid: u32,
) -> i32 {
    let Some(state) = rt_state() else { return -1 };
    state.identity = BoardIdentity { product_model, chip_idcode, chip_devid };
    0
}

/// The rustc target triple this crate was compiled for, from the build script -- the only statement
/// of the target ABI available to a source file. See `build.rs` for why no `cfg` can replace it.
const TARGET_TRIPLE: &str = env!("LAMELLA_TARGET_TRIPLE");

/// Declares how many bytes of update region the board's flash seam covers, and by doing so turns
/// firmware update on. Until this is called every `FW_WRITE` is refused and `FW_UPDATE` is not
/// advertised. Returns 0, or -1 before [`lamella_debug_rt_init_ram`].
///
/// # Why a length and no base
///
/// Every offset in the flash seam is RELATIVE to the update region, which is `FirmwareFlash`'s own
/// rule: *"every offset is relative to the region this implementor was constructed for, never an
/// absolute flash address."* So the board's seam owns the base -- it is the only thing that knows
/// where its linker script put the region -- and this layer needs only the extent, to bound a
/// chunk before handing it down.
#[cfg(feature = "fw-update")]
#[unsafe(no_mangle)]
pub extern "C" fn lamella_debug_rt_set_fw_region(len: u32) -> i32 {
    let Some(state) = rt_state() else { return -1 };
    state.image = lamella_fw_store::image::ImageStore::new(len as usize);
    state.fw_region = len;
    0
}

/// Declares the boot-locator region and which firmware slot is RUNNING, and by doing so turns
/// `FW_ACTIVATE` on. Until this is called activation is refused by name. Returns 0, or -1 before
/// [`lamella_debug_rt_init_ram`] or for a region too small to hold two locator slots.
///
/// # Why the running slot arrives here rather than being worked out
///
/// A host says `fw_slot::OTHER` so that it need not track which side the board is on, and that has
/// to be resolved against what is RUNNING -- not against what is recorded, because a pending
/// one-boot choice would make "the other one" mean a third thing. Only the code that chose this
/// image knows which side it booted from, so boot supplies it.
///
/// # Why a region too small is refused HERE
///
/// A locator slot is `ceil(payload / write_unit) + 1` units and the region holds TWO of them, so on
/// a part with a wide granule the floor is far above the payload -- 128 bytes for a four-byte
/// choice on a SAM D21. A region sized from the payload is short by more than 10x at the wrong end,
/// and discovering that at activation time means discovering it when a board needed recovering.
#[cfg(feature = "fw-update")]
#[unsafe(no_mangle)]
pub extern "C" fn lamella_debug_rt_set_locator_region(len: u32, running_slot: u8) -> i32 {
    let Some(state) = rt_state() else { return -1 };
    let unit = {
        use lamella_fw_store::FirmwareFlash;
        LocatorFlash.write_unit()
    };
    if unit == 0 || (len as usize) < 2 * lamella_fw_store::locator::slot_size(unit) {
        return -1;
    }
    state.locator_region = len;
    state.running_slot = running_slot;
    0
}

#[cfg(all(feature = "fw-update", not(test)))]
unsafe extern "C" {
    /// Bytes one program operation commits in the LOCATOR region.
    fn lamella_board_locator_write_unit() -> usize;
    /// Erases `[offset, offset + len)` of the locator region. 0 on success.
    fn lamella_board_locator_erase(offset: usize, len: usize) -> i32;
    /// Programs `len` bytes at `offset` of the locator region. 0 on success.
    fn lamella_board_locator_write(offset: usize, data: *const u8, len: usize) -> i32;
    /// Reads `len` bytes from `offset` of the locator region. 0 on success.
    fn lamella_board_locator_read(offset: usize, out: *mut u8, len: usize) -> i32;
    /// What one erased byte reads as in the locator region.
    fn lamella_board_locator_erased_byte() -> u8;
}

/// The board's LOCATOR region as a [`lamella_fw_store::FirmwareFlash`].
///
/// A second implementor rather than a region argument on the first, which is the fw-store's own
/// rule: *"a locator region and an image region are two implementors, and the arithmetic that turns
/// an offset into an address is the one place the region's base appears."* It is not tidiness --
/// the two regions can sit in different banks behind different controllers, so the WRITE UNIT is
/// per-region and a shared seam would have to answer one number for two questions.
#[cfg(feature = "fw-update")]
struct LocatorFlash;

#[cfg(feature = "fw-update")]
impl lamella_fw_store::FirmwareFlash for LocatorFlash {
    fn write_unit(&self) -> usize {
        #[cfg(not(test))]
        let unit = unsafe { lamella_board_locator_write_unit() };
        #[cfg(test)]
        let unit = tests::fake_locator_write_unit();
        unit
    }

    fn erase(&mut self, offset: usize, len: usize) -> Result<(), lamella_fw_store::FlashError> {
        #[cfg(not(test))]
        let rc = unsafe { lamella_board_locator_erase(offset, len) };
        #[cfg(test)]
        let rc = tests::fake_locator_erase(offset, len);
        if rc == 0 { Ok(()) } else { Err(lamella_fw_store::FlashError::Refused) }
    }

    fn write(&mut self, offset: usize, data: &[u8]) -> Result<(), lamella_fw_store::FlashError> {
        #[cfg(not(test))]
        let rc = unsafe { lamella_board_locator_write(offset, data.as_ptr(), data.len()) };
        #[cfg(test)]
        let rc = tests::fake_locator_write(offset, data);
        if rc == 0 { Ok(()) } else { Err(lamella_fw_store::FlashError::Refused) }
    }

    fn read(&self, offset: usize, out: &mut [u8]) -> Result<(), lamella_fw_store::FlashError> {
        #[cfg(not(test))]
        let rc = unsafe { lamella_board_locator_read(offset, out.as_mut_ptr(), out.len()) };
        #[cfg(test)]
        let rc = tests::fake_locator_read(offset, out);
        if rc == 0 { Ok(()) } else { Err(lamella_fw_store::FlashError::Refused) }
    }

    fn erased_byte(&self) -> u8 {
        #[cfg(not(test))]
        let byte = unsafe { lamella_board_locator_erased_byte() };
        #[cfg(test)]
        let byte = tests::fake_locator_erased_byte();
        byte
    }
}

#[cfg(all(feature = "fw-update", not(test)))]
unsafe extern "C" {
    /// Bytes one program operation commits. Every write is a whole number of these.
    fn lamella_board_fw_write_unit() -> usize;
    /// Erases `[offset, offset + len)` of the update region. 0 on success.
    fn lamella_board_fw_erase(offset: usize, len: usize) -> i32;
    /// Programs `len` bytes at `offset` of the update region. 0 on success.
    fn lamella_board_fw_write(offset: usize, data: *const u8, len: usize) -> i32;
    /// Reads `len` bytes from `offset` of the update region. 0 on success.
    fn lamella_board_fw_read(offset: usize, out: *mut u8, len: usize) -> i32;
    /// What one erased byte reads as -- `0xFF` on almost every part and `0x00` on an STM32L0.
    fn lamella_board_fw_erased_byte() -> u8;
}

/// The board's flash as a [`FirmwareFlash`], so the store's arithmetic runs against real silicon
/// without this archive naming a controller.
///
/// The same division as the carrier, for the same reason: which controller, which granule and where
/// the region sits are board facts, and this archive is built once per ISA for every board. What
/// stays here is the part that is the same everywhere -- the region bound, the read-back checksum
/// and the status ladder -- and all of that lives in `lamella-fw-store` already.
#[cfg(feature = "fw-update")]
struct BoardFlash;

#[cfg(feature = "fw-update")]
impl lamella_fw_store::FirmwareFlash for BoardFlash {
    fn write_unit(&self) -> usize {
        #[cfg(not(test))]
        let unit = unsafe { lamella_board_fw_write_unit() };
        #[cfg(test)]
        let unit = tests::fake_flash_write_unit();
        unit
    }

    fn erase(&mut self, offset: usize, len: usize) -> Result<(), lamella_fw_store::FlashError> {
        #[cfg(not(test))]
        let rc = unsafe { lamella_board_fw_erase(offset, len) };
        #[cfg(test)]
        let rc = tests::fake_flash_erase(offset, len);
        if rc == 0 { Ok(()) } else { Err(lamella_fw_store::FlashError::Refused) }
    }

    fn write(&mut self, offset: usize, data: &[u8]) -> Result<(), lamella_fw_store::FlashError> {
        #[cfg(not(test))]
        let rc = unsafe { lamella_board_fw_write(offset, data.as_ptr(), data.len()) };
        #[cfg(test)]
        let rc = tests::fake_flash_write(offset, data);
        if rc == 0 { Ok(()) } else { Err(lamella_fw_store::FlashError::Refused) }
    }

    fn read(&self, offset: usize, out: &mut [u8]) -> Result<(), lamella_fw_store::FlashError> {
        #[cfg(not(test))]
        let rc = unsafe { lamella_board_fw_read(offset, out.as_mut_ptr(), out.len()) };
        #[cfg(test)]
        let rc = tests::fake_flash_read(offset, out);
        if rc == 0 { Ok(()) } else { Err(lamella_fw_store::FlashError::Refused) }
    }

    fn erased_byte(&self) -> u8 {
        #[cfg(not(test))]
        let byte = unsafe { lamella_board_fw_erased_byte() };
        #[cfg(test)]
        let byte = tests::fake_flash_erased_byte();
        byte
    }
}

#[cfg(not(test))]
unsafe extern "C" {
    /// Bytes taken from the carrier into `buf` (at most `cap`), or 0 if none are ready. Supplied by
    /// the IMAGE: which peripheral carries Link is a board fact and this archive serves every board.
    fn lamella_board_link_read(buf: *mut u8, cap: usize) -> usize;
    /// Bytes of `buf` the carrier accepted, or 0 if it would have had to block.
    fn lamella_board_link_write(buf: *const u8, len: usize) -> usize;
}

/// Draw up to `cap` bytes from the board's carrier.
#[inline]
fn carrier_read(buf: &mut [u8]) -> usize {
    #[cfg(not(test))]
    let taken = unsafe { lamella_board_link_read(buf.as_mut_ptr(), buf.len()) };
    #[cfg(test)]
    let taken = tests::fake_carrier_read(buf);
    taken.min(buf.len())
}

/// Hand bytes to the board's carrier; answers how many it took.
#[inline]
fn carrier_write(buf: &[u8]) -> usize {
    #[cfg(not(test))]
    let sent = unsafe { lamella_board_link_write(buf.as_ptr(), buf.len()) };
    #[cfg(test)]
    let sent = tests::fake_carrier_write(buf);
    sent.min(buf.len())
}

/// The board's byte carrier as a [`Transport`]: the framing lives here, so the image supplies
/// bytes and never learns the protocol.
struct BoardCarrier;

impl Transport for BoardCarrier {
    fn send(&mut self, msg_type: u8, seq: u16, payload: &[u8]) -> Result<(), TransportError> {
        let frame = encode_frame(msg_type, seq, payload).ok_or(TransportError::PayloadTooLarge)?;
        let mut sent = 0;
        while sent < frame.len() {
            let wrote = carrier_write(&frame[sent..]);
            if wrote == 0 {
                return Err(TransportError::Carrier);
            }
            sent += wrote;
        }
        Ok(())
    }

    fn poll(&mut self) -> Result<Option<Frame>, TransportError> {
        let Some(state) = rt_state() else { return Ok(None) };
        let mut chunk = [0_u8; 64];
        loop {
            if let Some(frame) = state.reader.next_frame() {
                return Ok(Some(frame));
            }
            let read = carrier_read(&mut chunk);
            if read == 0 {
                return Ok(None);
            }
            state.reader.push(&chunk[..read]);
        }
    }
}

/// Serve whatever the carrier has ready, and answer it. Returns the number of frames served, or -1
/// before [`lamella_debug_rt_init_ram`].
///
/// The image calls this from wherever it can afford to: a main-loop poll, a timer, or a carrier
/// interrupt. It never blocks -- a poll with nothing ready serves 0 -- so hosting the link does not
/// change the timing of a program that nobody is debugging.
#[unsafe(no_mangle)]
pub extern "C" fn lamella_debug_rt_service() -> i32 {
    if rt_base() == 0 {
        return -1;
    }
    let mut served = 0_i32;
    let mut carrier = BoardCarrier;
    while let Ok(Some(frame)) = carrier.poll() {
        let mark = rt_mark();
        let outcome = dispatch(&mut carrier, &frame);
        drop(frame);
        unsafe { rt_release_to(mark) };
        if outcome.is_err() {
            break;
        }
        served = served.saturating_add(1);
    }
    served
}

/// What an AOT image advertises in its `HELLO_ACK`, with the window length supplied rather than
/// read from the state cell -- so the rule is checkable without a test installing a region, which
/// is process-wide and would decide the outcome of every other test depending on which ran first.
/// The same split, for the same reason, as [`checked_region`] and the agent's `checked_window`.
///
/// # The set is small, and the three bits a reader expects to find here are the interesting part
///
/// `PROFILE_CHIPID` is unconditional: it states that the identity carries STRUCTURED product and
/// chip fields rather than a display name to be parsed, which is true of every acknowledgement this
/// layer builds. Its own documentation says the fields may still read `0` = unknown, so an image
/// whose boot never called [`lamella_debug_rt_set_identity`] sets it and answers zeros -- exactly
/// what the interpreted tier does when nothing calls its `set_board_identity`.
///
/// `LIVE_MEMORY` is conditional on a window, and this is `deploy_caps_with`'s rule rather than a
/// second one: a capability bit is a promise a host ACTS on, so an image carrying the agent's code
/// but no declared window would advertise an op it refuses at every address.
///
/// **`MONOTONIC_CLOCK` is not set**, and cannot be from here. The bit has to be earned by a
/// positive observation of a clock ADVANCING, because a dead clock returns a well-formed number
/// that every caller believes; this layer observes no clock, so it makes no claim.
///
/// **`BAKED_IMAGE` and `DEBUG_BOOT_DEPLOYED` are not set.** Both describe loading and booting an
/// artifact over the wire. This image IS the artifact; there is nothing here to send one to.
///
/// **`ATTACH_NATIVE` is not set**, under a principle that outlives this one bit: *a capability bit
/// names an OPERATION SET a host may act on; it is set only when every operation the name implies
/// is answerable, and it never names a situation the target happens to be in.* `ATTACH_NATIVE`
/// reads as *attach to a running native program AND DRIVE IT*, and this image cannot be stopped, so
/// a host acting on it would offer a pause button nothing here answers. It stays clear while there
/// is no stop-and-resume path. The inspect-while-running half is already named by `LIVE_MEMORY`,
/// whose own documentation draws that line, and that is the bit this image earns.
fn image_caps_with(live_window_len: u32, fw_region_len: u32) -> lamella_wire::Capabilities {
    use lamella_wire::Capabilities;
    let live = if live_window_len == 0 { 0 } else { Capabilities::LIVE_MEMORY };
    let fw = if fw_region_len == 0 { 0 } else { Capabilities::FW_UPDATE };
    Capabilities(Capabilities::PROFILE_CHIPID | live | fw)
}

/// What this image IS, with the board facts supplied rather than read from the state cell -- the
/// same split as [`image_caps_with`], and checkable in one build for the same reason.
///
/// # Why the surface list is EMPTY, and why that is specified rather than chosen
///
/// A surface record describes a RESIDENT RUNTIME a target serves. This image has none: it is
/// compiled machine code, and the interpreter it would otherwise carry is the thing an AOT build
/// replaces. The wire format already names this case -- `TargetIdentity::surfaces` documents that
/// EMPTY means no resident interpreter, a bootloader or a target running native code, and that
/// WHICH of those it is, is read from the capability word. So the empty list is the format's own
/// answer, and the capability word beside it is what distinguishes us from a bootloader.
///
/// `firmware_version` stays `[0, 0]` -- unknown -- which is the honest answer for an image a
/// developer built. Filling it from a build system is the same question the interpreted tier's
/// `FIRMWARE_VERSION` leaves open, and inventing a number here would make every image claim to be
/// the same one.
fn image_identity(board: BoardIdentity) -> lamella_wire::TargetIdentity {
    lamella_wire::TargetIdentity {
        product_model: board.product_model,
        arch: lamella_wire::arch::from_target_triple(TARGET_TRIPLE),
        firmware_version: [0, 0],
        ..lamella_wire::TargetIdentity::default()
    }
    .with_chip_id(
        lamella_wire::chip_id_kind::DEBUG_PORT_AND_DEVICE_ID,
        &chip_identity_bytes(board.chip_idcode, board.chip_devid),
    )
}

/// The eight bytes of a `chip_id_kind::DEBUG_PORT_AND_DEVICE_ID` identity: the debug port's
/// identification code, then the vendor's device-id register, both little-endian.
fn chip_identity_bytes(idcode: u32, devid: u32) -> [u8; 8] {
    let mut bytes = [0u8; 8];
    bytes[0..4].copy_from_slice(&idcode.to_le_bytes());
    bytes[4..8].copy_from_slice(&devid.to_le_bytes());
    bytes
}

/// Answer a `HELLO`: negotiate the protocol version and advertise what this image serves.
///
/// The handshake belongs to the LINK layer rather than to the agent, so it is here unconditionally.
/// An image built without `debug-agent` still has to be discoverable and still has to agree a
/// protocol version -- firmware update rides on this same link, and a recovery path that cannot
/// handshake is not a recovery path.
fn serve_hello(
    carrier: &mut impl Transport,
    frame: &Frame,
) -> Result<(), TransportError> {
    use lamella_wire::{Hello, PROTOCOL_VERSION, ProtocolRange, msg, target_respond};
    let Some(hello) = Hello::decode(&frame.payload) else {
        return Ok(());
    };
    let (board, window_len, fw_len) = rt_state().map_or(
        (BoardIdentity::default(), 0, 0),
        |state| {
            #[cfg(feature = "debug-agent")]
            let window_len = state.window.1;
            #[cfg(not(feature = "debug-agent"))]
            let window_len = 0;
            #[cfg(feature = "fw-update")]
            let fw_len = state.fw_region;
            #[cfg(not(feature = "fw-update"))]
            let fw_len = 0;
            (state.identity, window_len, fw_len)
        },
    );
    let range = ProtocolRange { min: PROTOCOL_VERSION, max: PROTOCOL_VERSION };
    match target_respond(
        &hello,
        range,
        image_caps_with(window_len, fw_len),
        image_identity(board),
        carrier.max_inbound_payload(),
    ) {
        Ok(ack) => carrier.send(msg::HELLO_ACK, frame.seq, &ack.encode()),
        Err(nak) => carrier.send(msg::HELLO_NAK, frame.seq, &nak.encode()),
    }
}

/// Serve a `FW_WRITE`: program one chunk of the update region and answer what landed.
///
/// # Why a chunk at offset 0 erases
///
/// The block has no prepare op, and `ImageStore` refuses every chunk until the region has been
/// erased -- so something has to decide when that happens, and offset 0 is the only moment that
/// carries the information. It is also the rule the artifact blocks already follow: *a chunk at
/// offset 0 discards whatever partial transfer that destination held*. A host retrying from the
/// start therefore gets a clean region rather than its new bytes folded into an old checksum,
/// which is the failure the store's own `prepare` comment exists to prevent.
#[cfg(feature = "fw-update")]
fn serve_fw_write(
    carrier: &mut impl Transport,
    frame: &Frame,
) -> Result<(), TransportError> {
    use lamella_wire::msg::fw_write_status;
    let payload = &frame.payload;
    let Some(head) = payload.get(..4) else {
        return send_fw_result(carrier, frame.seq, fw_write_status::NOT_READY, 0, 0);
    };
    let offset = u32::from_le_bytes([head[0], head[1], head[2], head[3]]) as usize;
    let chunk = &payload[4..];

    let Some(state) = rt_state() else {
        return send_fw_result(carrier, frame.seq, fw_write_status::NOT_READY, 0, 0);
    };
    if state.fw_region == 0 {
        return send_fw_result(carrier, frame.seq, fw_write_status::NOT_READY, 0, 0);
    }

    let mut flash = BoardFlash;
    if offset == 0 && lamella_fw_store::image::ImageStore::prepare(&mut state.image, &mut flash).is_err() {
        return send_fw_result(carrier, frame.seq, fw_write_status::PROGRAM_FAILED, 0, 0);
    }

    let unit = {
        use lamella_fw_store::FirmwareFlash;
        flash.write_unit().max(1)
    };
    let padded = chunk.len().div_ceil(unit) * unit;
    let mut scratch = alloc::vec![0_u8; padded];
    let outcome = state.image.write_chunk(&mut flash, offset, chunk, &mut scratch);
    send_fw_result(carrier, frame.seq, outcome.status, outcome.accepted, outcome.running_crc)
}

/// `status(u8) | accepted(u32 LE) | running_crc(u32 LE)` -- the shape `lamella_wire_host::firmware`
/// reads back, in one place so the four refusal paths above cannot disagree about it.
#[cfg(feature = "fw-update")]
fn send_fw_result(
    carrier: &mut impl Transport,
    seq: u16,
    status: u8,
    accepted: u32,
    running_crc: u32,
) -> Result<(), TransportError> {
    let mut reply = [0_u8; 9];
    reply[0] = status;
    reply[1..5].copy_from_slice(&accepted.to_le_bytes());
    reply[5..9].copy_from_slice(&running_crc.to_le_bytes());
    carrier.send(lamella_wire::msg::FW_RESULT, seq, &reply)
}

/// Serve a `FW_COMMIT`: the host states what it believes it wrote, and the target agrees or refuses.
///
/// The expectation travels host to target on purpose -- a target that reported its own checksum and
/// a host that accepted it would agree by construction and verify nothing.
#[cfg(feature = "fw-update")]
fn serve_fw_commit(
    carrier: &mut impl Transport,
    frame: &Frame,
) -> Result<(), TransportError> {
    use lamella_wire::msg::{FW_COMMIT_RESULT, fw_commit_status};
    let request = frame.payload.get(..8).map(|at| {
        (
            u32::from_le_bytes([at[0], at[1], at[2], at[3]]),
            u32::from_le_bytes([at[4], at[5], at[6], at[7]]),
        )
    });
    let outcome = match (request, rt_state()) {
        (Some((total, expected)), Some(state)) => state.image.commit(total, expected),
        _ => lamella_fw_store::image::CommitOutcome {
            status: fw_commit_status::NOTHING_WRITTEN,
            image_crc: 0,
        },
    };
    let mut reply = [0_u8; 5];
    reply[0] = outcome.status;
    reply[1..5].copy_from_slice(&outcome.image_crc.to_le_bytes());
    carrier.send(FW_COMMIT_RESULT, frame.seq, &reply)
}

/// Serve a `FW_ACTIVATE`: record which installed image boots next, and for how long.
///
/// It takes effect at the NEXT boot -- activation records the choice and the running image keeps
/// running. A reset is a separate thing to ask for.
///
/// # What is refused, and one status this block cannot express
///
/// A slot that is neither 0, 1 nor [`fw_slot::OTHER`] is `NO_SUCH_SLOT`: the record stores the
/// resolved number as a byte and the boot path acts on it, so an out-of-range slot would be written
/// and then booted from nowhere.
///
/// **`SLOT_UNUSABLE` is also what a failed locator WRITE has to answer, and that is a
/// misattribution this layer cannot avoid.** `fw_activate_status` has no value meaning *the choice
/// could not be recorded* -- its refusals are all about the slot -- while the sibling
/// `fw_write_status` does carry `PROGRAM_FAILED`. So a worn or misconfigured locator region reports
/// as an unusable slot and sends a reader to look at the wrong flash. The reply's `next_slot`
/// carries the UNCHANGED choice in that case, which is the one thing that does tell a host the
/// activation did not take.
#[cfg(feature = "fw-update")]
fn serve_fw_activate(
    carrier: &mut impl Transport,
    frame: &Frame,
) -> Result<(), TransportError> {
    use lamella_wire::msg::{fw_activate_status, fw_intent, fw_slot};
    let Some(state) = rt_state() else {
        return carrier.send(
            lamella_wire::msg::ERROR,
            frame.seq,
            &lamella_wire::error::unknown_message_type(lamella_wire::msg::FW_ACTIVATE),
        );
    };
    let running = state.running_slot;
    let Some(&[slot, intent, ..]) = frame.payload.get(..2) else {
        return send_activation(carrier, frame.seq, fw_activate_status::NO_SUCH_SLOT, running, running);
    };

    if slot != fw_slot::OTHER && slot > 1 {
        return send_activation(carrier, frame.seq, fw_activate_status::NO_SUCH_SLOT, running, running);
    }

    let choice = lamella_fw_store::locator::BootChoice { next_slot: slot, intent };
    match lamella_fw_store::locator::activate(&mut LocatorFlash, choice, running) {
        Ok(resolved) => {
            let status = if resolved.intent == fw_intent::ONE_BOOT {
                fw_activate_status::ACTIVATED_ONE_BOOT
            } else {
                fw_activate_status::ACTIVATED
            };
            send_activation(carrier, frame.seq, status, running, resolved.next_slot)
        }
        Err(_) => {
            let unchanged = lamella_fw_store::locator::current(&LocatorFlash)
                .map_or(running, |(choice, _)| choice.next_slot);
            send_activation(carrier, frame.seq, fw_activate_status::SLOT_UNUSABLE, running, unchanged)
        }
    }
}

/// `status(u8) | active_slot(u8) | next_slot(u8)` -- the shape `lamella_wire_host::firmware` reads
/// back, in one place so the four paths above cannot disagree about it.
#[cfg(feature = "fw-update")]
fn send_activation(
    carrier: &mut impl Transport,
    seq: u16,
    status: u8,
    active_slot: u8,
    next_slot: u8,
) -> Result<(), TransportError> {
    carrier.send(lamella_wire::msg::FW_ACTIVATE_RESULT, seq, &[status, active_slot, next_slot])
}

/// Answer one frame.
///
/// # What is refused, and where the line is
///
/// Not "debug versus not" -- whether the op needs a RUNTIME. An AOT image owns no interpreter
/// session, so every op that dispatches through one (`DBG_EVAL`, `DBG_LOCALS`, the REPL block, the
/// LOAD block) has no meaning here. Those are declined in the CAPABILITY word at handshake time
/// rather than one at a time at run time, which is what [`image_caps_with`] is.
///
/// # And an op that arrives anyway is REFUSED rather than dropped
///
/// The capability word tells a host what not to send; it does not stop one. A dropped frame reaches
/// the host as a TIMEOUT, and a timeout cannot be told apart from a board that has stopped
/// answering -- which on an image whose whole purpose is to be debugged is the single most
/// expensive ambiguity there is. The three explanations for silence are "I do not implement that",
/// "I crashed" and "the cable is bad", and they are three different repairs.
fn dispatch(carrier: &mut impl Transport, frame: &Frame) -> Result<(), TransportError> {
    match frame.msg_type {
        lamella_wire::msg::PING => carrier.send(lamella_wire::msg::PONG, frame.seq, &[]),
        lamella_wire::msg::HELLO => serve_hello(carrier, frame),
        #[cfg(feature = "fw-update")]
        lamella_wire::msg::FW_WRITE => serve_fw_write(carrier, frame),
        #[cfg(feature = "fw-update")]
        lamella_wire::msg::FW_COMMIT => serve_fw_commit(carrier, frame),
        #[cfg(feature = "fw-update")]
        lamella_wire::msg::FW_ACTIVATE
            if rt_state().is_some_and(|state| state.locator_region != 0) =>
        {
            serve_fw_activate(carrier, frame)
        }
        #[cfg(feature = "debug-agent")]
        msg_type if lamella_debug_agent::live::is_request(msg_type) => {
            let window = rt_state().map_or((0, 0), |state| state.window);
            lamella_debug_agent::serve_live_frame(
                carrier,
                frame,
                window,
                &mut lamella_debug_agent::TargetMemory,
            )
        }
        other => carrier.send(
            lamella_wire::msg::ERROR,
            frame.seq,
            &lamella_wire::error::unknown_message_type(other),
        ),
    }
}

#[doc(hidden)]
pub fn __lamella_force_link() -> usize {
    lamella_debug_rt_service as *const () as usize
}

#[cfg(test)]
mod tests {
    //! The region rule, which is the part that is testable without a board.
    use super::{
        BoardIdentity, RtState, STATE_OFF, TARGET_TRIPLE, checked_region, image_caps_with,
        image_identity, lamella_debug_rt_init_ram, lamella_debug_rt_service,
        lamella_debug_rt_set_identity, rt_alloc,
    };
    use lamella_wire::Capabilities;
    use std::sync::Mutex;

    /// Bytes waiting to be READ by the layer, and bytes it has WRITTEN.
    static CARRIER: Mutex<(Vec<u8>, Vec<u8>)> = Mutex::new((Vec::new(), Vec::new()));

    pub(super) fn fake_carrier_read(buf: &mut [u8]) -> usize {
        let mut carrier = CARRIER.lock().unwrap();
        let take = carrier.0.len().min(buf.len());
        buf[..take].copy_from_slice(&carrier.0[..take]);
        carrier.0.drain(..take);
        take
    }

    /// Where the last carrier write allocated from the LINK REGION, or 0 with none installed.
    ///
    static PROBE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    pub(super) fn fake_carrier_write(buf: &[u8]) -> usize {
        let word = core::alloc::Layout::from_size_align(8, 8).expect("a word is a valid layout");
        PROBE.store(super::rt_alloc(word) as usize, std::sync::atomic::Ordering::Relaxed);
        CARRIER.lock().unwrap().1.extend_from_slice(buf);
        buf.len()
    }

    /// A fake update region with a REAL write granule, so the store's alignment and padding rules
    /// are exercised rather than assumed.
    ///
    #[cfg(feature = "fw-update")]
    pub(super) const FAKE_UNIT: usize = 8;
    #[cfg(feature = "fw-update")]
    pub(super) static FLASH: Mutex<Vec<u8>> = Mutex::new(Vec::new());

    #[cfg(feature = "fw-update")]
    pub(super) fn fake_flash_write_unit() -> usize {
        FAKE_UNIT
    }

    #[cfg(feature = "fw-update")]
    pub(super) fn fake_flash_erased_byte() -> u8 {
        0xFF
    }

    #[cfg(feature = "fw-update")]
    pub(super) fn fake_flash_erase(offset: usize, len: usize) -> i32 {
        let mut flash = FLASH.lock().unwrap();
        if offset + len > flash.len() {
            return -1;
        }
        flash[offset..offset + len].fill(0xFF);
        0
    }

    #[cfg(feature = "fw-update")]
    pub(super) fn fake_flash_write(offset: usize, data: &[u8]) -> i32 {
        let mut flash = FLASH.lock().unwrap();
        if offset + data.len() > flash.len() {
            return -1;
        }
        flash[offset..offset + data.len()].copy_from_slice(data);
        0
    }

    #[cfg(feature = "fw-update")]
    pub(super) fn fake_flash_read(offset: usize, out: &mut [u8]) -> i32 {
        let flash = FLASH.lock().unwrap();
        if offset + out.len() > flash.len() {
            return -1;
        }
        out.copy_from_slice(&flash[offset..offset + out.len()]);
        0
    }

    /// The locator region, with a granule DELIBERATELY UNLIKE the image region's.
    ///
    #[cfg(feature = "fw-update")]
    pub(super) const FAKE_LOCATOR_UNIT: usize = 4;
    #[cfg(feature = "fw-update")]
    pub(super) static LOCATOR: Mutex<Vec<u8>> = Mutex::new(Vec::new());

    #[cfg(feature = "fw-update")]
    pub(super) fn fake_locator_write_unit() -> usize {
        FAKE_LOCATOR_UNIT
    }

    #[cfg(feature = "fw-update")]
    pub(super) fn fake_locator_erased_byte() -> u8 {
        0xFF
    }

    #[cfg(feature = "fw-update")]
    pub(super) fn fake_locator_erase(offset: usize, len: usize) -> i32 {
        let mut flash = LOCATOR.lock().unwrap();
        if offset + len > flash.len() {
            return -1;
        }
        flash[offset..offset + len].fill(0xFF);
        0
    }

    #[cfg(feature = "fw-update")]
    pub(super) fn fake_locator_write(offset: usize, data: &[u8]) -> i32 {
        let mut flash = LOCATOR.lock().unwrap();
        if offset + data.len() > flash.len() {
            return -1;
        }
        flash[offset..offset + data.len()].copy_from_slice(data);
        0
    }

    #[cfg(feature = "fw-update")]
    pub(super) fn fake_locator_read(offset: usize, out: &mut [u8]) -> i32 {
        let flash = LOCATOR.lock().unwrap();
        if offset + out.len() > flash.len() {
            return -1;
        }
        out.copy_from_slice(&flash[offset..offset + out.len()]);
        0
    }

    /// The first byte the heap may hand out, for a region installed at `base`.
    fn heap_floor(base: usize) -> usize {
        base + STATE_OFF + core::mem::size_of::<RtState>()
    }

    #[test]
    fn a_region_too_small_for_the_state_is_refused_rather_than_trimmed() {
        assert_eq!(checked_region(0x2000_0000, 8), None);
        assert_eq!(checked_region(0x2000_0000, STATE_OFF), None);
        assert_eq!(checked_region(0x2000_0000, heap_floor(0)), None);
        assert!(checked_region(0x2000_0000, 4096).is_some());
    }

    #[test]
    fn the_alignment_adjustment_comes_out_of_the_length_rather_than_off_the_end() {
        let just_enough = heap_floor(0) + 1;
        assert!(checked_region(0x2000_0000, just_enough).is_some());
        assert_eq!(checked_region(0x2000_0001, just_enough), None);
        assert_eq!(checked_region(0x2000_0001, 4096).map(|region| region.0), Some(0x2000_0008));
    }

    #[test]
    fn the_live_bit_is_earned_by_a_window_and_nothing_else_is_claimed() {
        assert!(!image_caps_with(0, 0).has(Capabilities::LIVE_MEMORY));
        assert!(image_caps_with(1024, 0).has(Capabilities::LIVE_MEMORY));

        assert!(!image_caps_with(1024, 0).has(Capabilities::ATTACH_NATIVE));
        assert!(!image_caps_with(1024, 0).has(Capabilities::ATTACH_INTERPRETED));

        assert!(!image_caps_with(1024, 0).has(Capabilities::MONOTONIC_CLOCK));
        assert!(!image_caps_with(1024, 0).has(Capabilities::BAKED_IMAGE));
        assert!(!image_caps_with(1024, 0).has(Capabilities::DEBUG_BOOT_DEPLOYED));

        assert!(image_caps_with(0, 0).has(Capabilities::PROFILE_CHIPID));

        assert!(!image_caps_with(0, 0).has(Capabilities::FW_UPDATE));
        assert!(image_caps_with(0, 64 * 1024).has(Capabilities::FW_UPDATE));
        assert!(!image_caps_with(0, 64 * 1024).has(Capabilities::LIVE_MEMORY));
        assert!(!image_caps_with(1024, 0).has(Capabilities::FW_UPDATE));
    }

    #[test]
    fn an_image_advertises_no_resident_runtime_and_a_real_architecture() {
        let identity = image_identity(BoardIdentity {
            product_model: 0x1234,
            chip_idcode: 0x2ba0_1477,
            chip_devid: 0x6000_0000,
        });

        assert!(identity.surfaces.is_empty(), "an AOT image serves no resident runtime");
        assert_eq!(identity.firmware_version, [0, 0], "unknown, which is honest for a dev build");

        assert_eq!(identity.product_model, 0x1234);
        assert_eq!(identity.chip_id_kind, lamella_wire::chip_id_kind::DEBUG_PORT_AND_DEVICE_ID);
        assert_eq!(identity.chip_id, [0x77, 0x14, 0xa0, 0x2b, 0x00, 0x00, 0x00, 0x60]);

        assert!(!TARGET_TRIPLE.is_empty(), "the build script always sets it");
        for triple in ["thumbv6m-none-eabi", "thumbv7em-none-eabi", "thumbv7em-none-eabihf"] {
            assert_ne!(
                lamella_wire::arch::from_target_triple(triple),
                lamella_wire::arch::UNKNOWN,
                "{triple} is a target this archive is cross-built for"
            );
        }
        assert_eq!(identity.arch, lamella_wire::arch::from_target_triple(TARGET_TRIPLE));
    }

    /// THE ONE THAT SAYS AN IMAGE HOSTS THE LINK: bytes in, a real answer out.
    ///
    /// A PING is the smallest op that proves the whole path, and every part of it is the shipped
    /// code: the reader resynchronizes on SYNC, the CRC is checked, the type is dispatched, and the
    /// reply is framed and handed back to the carrier. Nothing here is the interpreter's -- this is
    /// the first time a native image has answered a Lamella Link frame.
    ///
    #[test]
    fn the_layer_is_off_until_a_region_is_installed_and_then_completes_a_handshake() {
        assert_eq!(lamella_debug_rt_service(), -1);

        let region: &'static mut [u8] = Box::leak(vec![0_u8; 8192].into_boxed_slice());
        let base = region.as_mut_ptr() as usize;
        assert_eq!(unsafe { lamella_debug_rt_init_ram(base, region.len()) }, 0);

        let layout = core::alloc::Layout::from_size_align(16, 8).unwrap();
        let first = rt_alloc(layout);
        assert!(!first.is_null());
        assert!(first as usize >= heap_floor((base + 7) & !7));

        let ping = lamella_wire::encode_frame(lamella_wire::msg::PING, 7, &[]).expect("frames");
        {
            let mut carrier = CARRIER.lock().unwrap();
            carrier.0.extend_from_slice(&ping);
            carrier.1.clear();
        }

        assert_eq!(lamella_debug_rt_service(), 1, "one frame arrived, so one is served");

        let written = core::mem::take(&mut CARRIER.lock().unwrap().1);
        let mut reader = lamella_wire::FrameReader::new();
        reader.push(&written);
        let reply = reader.next_frame().expect("the reply is a whole, CRC-valid frame");
        assert_eq!(reply.msg_type, lamella_wire::msg::PONG);
        assert_eq!(reply.seq, 7, "a reply carries its request's sequence number");
        assert!(reply.payload.is_empty());

        assert!(reader.next_frame().is_none());
        assert_eq!(lamella_debug_rt_service(), 0);

        assert!(!rt_alloc(core::alloc::Layout::from_size_align(1024, 8).unwrap()).is_null());


        assert_eq!(lamella_debug_rt_set_identity(0x1234, 0x2ba0_1477, 0x6000_0000), 0);
        #[cfg(feature = "debug-agent")]
        assert_eq!(super::lamella_debug_rt_set_window(0x2000_0000, 1024), 0);

        let hello = lamella_wire::Hello {
            range: lamella_wire::ProtocolRange::default(),
            caps: lamella_wire::Capabilities(u64::MAX),
        };
        let reply = exchange(lamella_wire::msg::HELLO, 11, &hello.encode());
        assert_eq!(reply.msg_type, lamella_wire::msg::HELLO_ACK);
        assert_eq!(reply.seq, 11, "a reply answers its request's sequence");

        let ack = lamella_wire::HelloAck::decode(&reply.payload).expect("a host decodes the ack");
        assert_eq!(ack.chosen, lamella_wire::PROTOCOL_VERSION);
        assert!(ack.identity.surfaces.is_empty(), "no resident runtime on this tier");
        assert_eq!(ack.identity.product_model, 0x1234, "the image's own board fact, round-tripped");
        assert_eq!(ack.identity.chip_id, [0x77, 0x14, 0xa0, 0x2b, 0x00, 0x00, 0x00, 0x60]);

        #[cfg(feature = "debug-agent")]
        assert!(ack.caps.has(lamella_wire::Capabilities::LIVE_MEMORY), "the window was declared");
        #[cfg(not(feature = "debug-agent"))]
        assert!(
            !ack.caps.has(lamella_wire::Capabilities::LIVE_MEMORY),
            "an image built without the agent advertises no live ops, however it was booted"
        );
        assert!(!ack.caps.has(lamella_wire::Capabilities::ATTACH_NATIVE));

        let negotiated = lamella_wire::host_finish(&ack, lamella_wire::Capabilities(u64::MAX));
        assert_eq!(negotiated.version, lamella_wire::PROTOCOL_VERSION);
        assert_eq!(negotiated.target_caps, ack.caps);

        let after_first = PROBE.load(std::sync::atomic::Ordering::Relaxed);
        assert_ne!(after_first, 0, "the probe really did allocate from the region");
        let _ = exchange(lamella_wire::msg::HELLO, 99, &hello.encode());
        assert_eq!(
            PROBE.load(std::sync::atomic::Ordering::Relaxed),
            after_first,
            "a second handshake reused the first one's bytes rather than taking new ones"
        );

        #[cfg(feature = "fw-update")]
        {
            use lamella_wire::msg::{fw_commit_status, fw_write_status};
            const REGION: usize = 64;
            *FLASH.lock().unwrap() = vec![0x00; REGION];
            assert_eq!(super::lamella_debug_rt_set_fw_region(REGION as u32), 0);

            let first: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];
            let second: [u8; 5] = [9, 10, 11, 12, 13];

            let mut request = 0u32.to_le_bytes().to_vec();
            request.extend_from_slice(&first);
            let reply = exchange(lamella_wire::msg::FW_WRITE, 20, &request);
            assert_eq!(reply.msg_type, lamella_wire::msg::FW_RESULT);
            assert_eq!(reply.payload[0], fw_write_status::WRITTEN, "the first chunk landed");

            let mut request = 8u32.to_le_bytes().to_vec();
            request.extend_from_slice(&second);
            let reply = exchange(lamella_wire::msg::FW_WRITE, 21, &request);
            assert_eq!(reply.payload[0], fw_write_status::WRITTEN, "and so did the padded tail");
            let running = u32::from_le_bytes(reply.payload[5..9].try_into().unwrap());

            assert_eq!(&FLASH.lock().unwrap()[0..13], &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13]);

            let mut expect = first.to_vec();
            expect.extend_from_slice(&second);
            assert_eq!(expect.len(), 13, "the image is not a whole number of granules, on purpose");
            assert_eq!(running, lamella_fw_store::crc::of(&expect), "read back, not accumulated");

            let mut bad = 13u32.to_le_bytes().to_vec();
            bad.extend_from_slice(&running.wrapping_add(1).to_le_bytes());
            let reply = exchange(lamella_wire::msg::FW_COMMIT, 22, &bad);
            assert_eq!(reply.msg_type, lamella_wire::msg::FW_COMMIT_RESULT);
            assert_eq!(reply.payload[0], fw_commit_status::CHECKSUM_MISMATCH);

            let mut good = 13u32.to_le_bytes().to_vec();
            good.extend_from_slice(&running.to_le_bytes());
            let reply = exchange(lamella_wire::msg::FW_COMMIT, 23, &good);
            assert_eq!(reply.payload[0], fw_commit_status::COMMITTED, "the image is a candidate");

            let hello = lamella_wire::Hello {
                range: lamella_wire::ProtocolRange::default(),
                caps: lamella_wire::Capabilities(u64::MAX),
            };
            let reply = exchange(lamella_wire::msg::HELLO, 24, &hello.encode());
            let ack = lamella_wire::HelloAck::decode(&reply.payload).expect("an ack");
            assert!(ack.caps.has(lamella_wire::Capabilities::FW_UPDATE));
        }

        #[cfg(feature = "fw-update")]
        {
            use lamella_wire::msg::{fw_activate_status, fw_intent, fw_slot};

            let refused = exchange(lamella_wire::msg::FW_ACTIVATE, 30, &[fw_slot::OTHER, 0]);
            assert_eq!(refused.msg_type, lamella_wire::msg::ERROR);
            assert_eq!(
                refused.payload,
                lamella_wire::error::unknown_message_type(lamella_wire::msg::FW_ACTIVATE)
            );

            let slot_size = lamella_fw_store::locator::slot_size(FAKE_LOCATOR_UNIT);
            assert_eq!(slot_size, 8, "ceil(4/4) + 1 units, on a four-byte granule");
            *LOCATOR.lock().unwrap() = vec![0xFF; 4 * slot_size];

            assert_eq!(
                super::lamella_debug_rt_set_locator_region((2 * slot_size - 1) as u32, 0),
                -1
            );
            assert_eq!(super::lamella_debug_rt_set_locator_region((4 * slot_size) as u32, 0), 0);

            let reply = exchange(
                lamella_wire::msg::FW_ACTIVATE,
                31,
                &[fw_slot::OTHER, fw_intent::PERMANENT],
            );
            assert_eq!(reply.msg_type, lamella_wire::msg::FW_ACTIVATE_RESULT);
            assert_eq!(reply.payload[0], fw_activate_status::ACTIVATED);
            assert_eq!(reply.payload[1], 0, "slot 0 is the one running");
            assert_eq!(reply.payload[2], 1, "so the other one is slot 1");

            let held = lamella_fw_store::locator::current(&super::LocatorFlash)
                .expect("a complete record is in force");
            assert_eq!(held.0.next_slot, 1);
            assert_eq!(held.0.intent, fw_intent::PERMANENT);

            let reply = exchange(
                lamella_wire::msg::FW_ACTIVATE,
                32,
                &[1, fw_intent::ONE_BOOT],
            );
            assert_eq!(reply.payload[0], fw_activate_status::ACTIVATED_ONE_BOOT);

            let reply = exchange(lamella_wire::msg::FW_ACTIVATE, 33, &[7, fw_intent::PERMANENT]);
            assert_eq!(reply.payload[0], fw_activate_status::NO_SUCH_SLOT);
            assert_eq!(
                lamella_fw_store::locator::current(&super::LocatorFlash).expect("still a record").0.next_slot,
                1,
                "a refused activation changed nothing"
            );
        }

        {
            let mut stream = vec![0x4C, 0x57];
            stream.extend_from_slice(&60_000_u16.to_le_bytes());
            stream.extend_from_slice(&[lamella_wire::msg::PING, 0, 0]);
            stream.extend(
                lamella_wire::encode_frame(lamella_wire::msg::PING, 42, &[]).expect("frames"),
            );
            {
                let mut carrier = CARRIER.lock().unwrap();
                carrier.0.extend_from_slice(&stream);
                carrier.1.clear();
            }
            assert_eq!(lamella_debug_rt_service(), 1, "the good frame behind the garbage is served");
            let written = core::mem::take(&mut CARRIER.lock().unwrap().1);
            let mut reader = lamella_wire::FrameReader::new();
            reader.push(&written);
            let reply = reader.next_frame().expect("a whole, CRC-valid reply");
            assert_eq!(reply.msg_type, lamella_wire::msg::PONG);
            assert_eq!(reply.seq, 42, "and it is the frame BEHIND the bad header that answered");
        }

        let refused = exchange(lamella_wire::msg::DBG_PAUSE, 12, &[]);
        assert_eq!(refused.msg_type, lamella_wire::msg::ERROR);
        assert_eq!(refused.seq, 12);
        assert_eq!(
            refused.payload,
            lamella_wire::error::unknown_message_type(lamella_wire::msg::DBG_PAUSE),
            "the refusal names the type that was not understood"
        );
    }

    /// Put one frame on the carrier, service it, and decode the single frame that comes back.
    ///
    fn exchange(msg_type: u8, seq: u16, payload: &[u8]) -> lamella_wire::Frame {
        let request = lamella_wire::encode_frame(msg_type, seq, payload).expect("frames");
        {
            let mut carrier = CARRIER.lock().unwrap();
            carrier.0.extend_from_slice(&request);
            carrier.1.clear();
        }
        assert_eq!(lamella_debug_rt_service(), 1, "one frame arrived, so one is served");

        let written = core::mem::take(&mut CARRIER.lock().unwrap().1);
        let mut reader = lamella_wire::FrameReader::new();
        reader.push(&written);
        let reply = reader.next_frame().expect("the reply is a whole, CRC-valid frame");
        assert!(reader.next_frame().is_none(), "exactly one frame answered the request");
        reply
    }
}
