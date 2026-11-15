//! The network stored on the board: one record in two slots, each slot its own erase unit.
//!
//! A new record is written into the slot that does not hold the current one, read back, and only
//! then is the other slot ERASED -- so the name and secret a record replaces never survive it, on any
//! flash part, without programming a written location twice. Power lost between the two steps leaves
//! two undamaged copies; the higher sequence number wins, and the next read erases the other. A slot
//! that is neither erased nor undamaged -- a write cut short -- is erased when found, since its bytes
//! may hold part of a secret.
//!
//! A slot starts with format 1, 192 bytes at the start of its erase unit, every number little-endian:
//!
//! | Offset | Bytes | Field |
//! |---|---|---|
//! | 0 | 4 | magic, `LWFR` |
//! | 4 | 1 | format version, 1 |
//! | 5 | 1 | the security kinds a join accepts, as bits (open 1, WPA2 2, WPA3 4) |
//! | 6 | 1 | the name's length, 1 to 32 |
//! | 7 | 1 | the secret's length, 0 to 128 |
//! | 8 | 4 | sequence number: the higher valid slot wins |
//! | 12 | 32 | the name, zero past its length |
//! | 44 | 128 | the secret, zero past its length |
//! | 172 | 1 | reconnection: 0 automatic, 1 manual |
//! | 173 | 1 | boot: 0 join in the background, 2 join on request; 1, which was "join before the program starts", is retired (below) |
//! | 174 | 14 | reserved, zero |
//! | 188 | 4 | CRC-32 (IEEE 802.3) of the 188 bytes before it |
//!
//! **Every format keeps format 1's frame:** the magic at 0, the format byte at 4, the sequence number
//! at 8 to 11, and at 188 the CRC-32 of the 188 bytes before it. A later format may change what lies
//! between those fields and what follows the frame. So a reader checks the frame and reads the
//! sequence number of a copy in any format, one it does not know included, and judges a copy in a
//! format it does not know by that frame alone: what follows it is that format's.
//!
//! In format 1, later records may follow the frame, the first at offset 192 and each next one right
//! after the one before. A later record is its kind's four-byte magic, a format byte, its whole length
//! in two bytes (11 or more, the magic through the CRC-32), its body, and the CRC-32 of every byte of
//! it before the CRC. The list ends where the next magic would start and reads `FF FF FF FF`, or at the
//! unit's end; no later record takes format 1's magic. A format 1 slot is undamaged only when its
//! frame and every later record in it verify, so a later record cut short is never taken as current.
//!
//! This writer knows no later record, so it copies every one of a format 1 copy forward, byte for
//! byte, into the new slot; over a copy in a format it does not know, it carries nothing of that copy.
//! The flash is programmed a page at a time, each page once: the pages after the first go
//! first, and the first page, which holds format 1, goes last. So a slot carries format 1's magic
//! only once everything after it is in place.
//!
//! An erased slot reads `0xFF` and a zeroed one `0x00`, and neither carries the magic, so neither
//! holds a record.
//!
//! No boot value holds the program's start for the network. A slot whose boot byte is 1 is still
//! valid, and the board follows it as 0: the program starts at once and the network is joined in the
//! background. Every write stores 0 in its place, a network stored over it as much as a changed boot
//! setting. A boot byte past 2 names no setting this reader knows (the two copies, below).
//!
//! **The two copies.** A copy is damaged when its frame fails its check, or, in format 1, when a later
//! record in it fails its check or its list of later records does not end as above: a write cut short.
//! A damaged copy is never
//! current: the other copy is, when it is undamaged, and the damaged one is erased when found. Among
//! the undamaged copies, the higher sequence number is current, whether or not this reader can use
//! every value in it. A value this reader cannot use -- a format other than 1, a security kind, a
//! reconnection or a boot value outside those above, a name of 0 or more than 32 bytes, a secret of
//! more than 128 -- marks a copy a newer version stored. Then the board joins no stored network, never the older copy, and
//! says so in one line; the copy is kept as it is, for a firmware that can use it, and the older copy
//! is erased as replaced. A program can still join a network by name, and storing a network replaces
//! the copy as any write does. Nothing checks that a secret fits its kind: the `lamella` tool refuses
//! such a secret when it stores one. The erases at a start are this reader's: Lamella RHU's kernel
//! takes the same copy as current, but writes no flash at a start, so it leaves both copies as they
//! are.
//!
//! A join from the record chooses its security kind from the kinds the record accepts:
//!
//! - **One kind:** that kind alone, with no scan, and a failed join is never tried again as another
//!   kind. A join that accepts WPA3 alone, on a radio whose firmware does not run WPA3's SAE exchange,
//!   is not tried: it fails, and says why.
//! - **WPA2 and WPA3:** the radio is asked whether its firmware runs SAE. If it does not, the network
//!   is joined with WPA2, with no scan. If it does, the network is scanned for by its name: when the
//!   scan sees it offer WPA3 it is joined with WPA3, and when the scan sees it offer WPA2 alone, with
//!   WPA2. A background join that sees neither rests and scans again; a program's own join returns
//!   the mismatch.
//! - **Open** is accepted only alone: a record that accepts open beside a secured kind is one this
//!   reader cannot use (the two copies, above).
//! - **A lost link,** with reconnection 0, is joined again by the radio with the kind in force. When
//!   the radio's scan for it sees the network offering no suite that kind can use, a record that
//!   accepts WPA2 and WPA3 is joined again with the kind the network now offers, chosen as above;
//!   a background join with any other record rests and keeps looking for a compatible network.
//!   A program's own join that cannot change to an accepted kind releases the network.
//!
//! A scan that carries the name transmits the stored network's name: at every look for a network
//! that hides its name, and for one that broadcasts its name only while it is out of range.
//!
//! **The three settings.** Whether a network is stored is chosen when it is joined: one that is not
//! stored is joined that once. The start setting says what the board does when it starts: join the
//! stored network in the background, beside the program -- a join that keeps trying until the
//! network is there -- or join it only when the program asks. Reconnection says what the board does
//! after a working link drops, and only then: join again by itself, or leave the next join to the
//! program. A background join retries a refused secret after rests of 5, 10, 20, 40, 80, 160, 300
//! seconds, then 300 seconds without a limit on attempts. A hardware random seed, drawn once when
//! the radio starts, adds up to 20% to each rest when available. A completed join resets the next
//! rest to five seconds. WPA2 reason 15 and any failed WPA3 authentication start these rests; a
//! WPA3 failure does not prove that the secret was wrong. A program's own join, including one
//! reading this record, returns the refusal at once. The common setups:
//!
//! - **Connect just this once:** joined, not stored, and not joined again after a drop.
//! - **Remember the network and stay on it,** the default: stored, joined in the background at start
//!   (boot 0), and joined again by itself (reconnection 0).
//! - **Join only when the program has data:** stored, and joined when the program asks (boot 2).
//!

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use lamella_net_core::wifi::{security, BootConnection, Reconnection, RecordWrite, SECRET_MAX, SSID_MAX};

/// The bytes of format 1, the record every slot starts with.
pub const SLOT_LEN: usize = 192;

/// The bytes the storage programs at once: a page of the flash, at a page boundary of the unit.
pub const PAGE: usize = 256;

/// The slots: two.
pub const SLOTS: usize = 2;

/// The bytes of the smallest later record: a magic, a format byte, a length and a CRC-32.
const LATER_MIN: usize = 4 + 1 + 2 + 4;

const MAGIC: [u8; 4] = *b"LWFR";
const VERSION: u8 = 1;
const AT_VERSION: usize = 4;
const AT_SECURITY: usize = 5;
const AT_NAME_LEN: usize = 6;
const AT_SECRET_LEN: usize = 7;
const AT_SEQUENCE: usize = 8;
const AT_NAME: usize = 12;
const AT_SECRET: usize = 44;
const AT_RECONNECTION: usize = 172;
const AT_BOOT: usize = 173;
const AT_CHECK: usize = 188;

/// The storage under the record: two erase units, one slot each.
pub trait RecordStore {
    /// Slot `slot`'s whole erase unit (slot 0 or 1), as the storage holds it now. Its length is the
    /// unit's, at least one [`PAGE`].
    fn read(&self, slot: usize) -> &[u8];

    /// Erases slot `slot`'s whole unit. Returns whether it then reads erased.
    fn erase(&mut self, slot: usize) -> bool;

    /// Programs one page, `page`, at byte `offset` of slot `slot`'s unit: a multiple of [`PAGE`],
    /// where the unit reads erased. Returns whether the storage took the write.
    fn program(&mut self, slot: usize, offset: usize, page: &[u8; PAGE]) -> bool;

    /// Whether this storage keeps records at all: true for a board's own storage, even while it holds
    /// none, and false for [`NoStore`]. A firmware built over a store that keeps none reads no stored
    /// network, whatever the board holds, and can say so rather than that none is stored.
    fn keeps_records(&self) -> bool {
        true
    }
}

/// A store behind a box, for a holder that names the store's type only when it is built.
impl<T: RecordStore + ?Sized> RecordStore for alloc::boxed::Box<T> {
    fn read(&self, slot: usize) -> &[u8] {
        (**self).read(slot)
    }

    fn erase(&mut self, slot: usize) -> bool {
        (**self).erase(slot)
    }

    fn program(&mut self, slot: usize, offset: usize, page: &[u8; PAGE]) -> bool {
        (**self).program(slot, offset, page)
    }

    fn keeps_records(&self) -> bool {
        (**self).keeps_records()
    }
}

/// A store with nothing in it that takes no write, for a firmware that keeps no board settings: it
/// holds no network, and it keeps no records, so the firmware reads none from the board.
pub struct NoStore;

impl RecordStore for NoStore {
    fn read(&self, _slot: usize) -> &[u8] {
        &[0xFF; PAGE]
    }

    fn erase(&mut self, _slot: usize) -> bool {
        true
    }

    fn program(&mut self, _slot: usize, _offset: usize, _page: &[u8; PAGE]) -> bool {
        false
    }

    fn keeps_records(&self) -> bool {
        false
    }
}

/// A stored network, held in memory: its secret is erased when the value is dropped.
#[derive(Clone)]
pub struct Record {
    ssid: [u8; SSID_MAX],
    ssid_len: usize,
    secret: [u8; SECRET_MAX],
    secret_len: usize,
    /// The security kinds a join from the record accepts, as bits.
    pub security: u8,
    /// What the radio does when the link is lost.
    pub reconnection: Reconnection,
    /// What the network does when the board starts.
    pub boot: BootConnection,
    sequence: u32,
}

impl Record {
    /// A record of a network; `None` for a name outside 1 to 32 bytes, a secret past 128 bytes, or a
    /// set of kinds no join can take.
    #[must_use]
    pub fn new(
        ssid: &[u8],
        secret: &[u8],
        security: u8,
        reconnection: Reconnection,
        boot: BootConnection,
    ) -> Option<Self> {
        if ssid.is_empty() || ssid.len() > SSID_MAX || secret.len() > SECRET_MAX {
            return None;
        }
        if !kinds_valid(security) {
            return None;
        }
        let mut record = Record {
            ssid: [0; SSID_MAX],
            ssid_len: ssid.len(),
            secret: [0; SECRET_MAX],
            secret_len: secret.len(),
            security,
            reconnection,
            boot,
            sequence: 0,
        };
        record.ssid[..ssid.len()].copy_from_slice(ssid);
        record.secret[..secret.len()].copy_from_slice(secret);
        Some(record)
    }

    /// The network's name.
    #[must_use]
    pub fn ssid(&self) -> &[u8] {
        &self.ssid[..self.ssid_len]
    }

    /// The secret. Read only by the code that hands it to the radio.
    #[must_use]
    pub fn secret(&self) -> &[u8] {
        &self.secret[..self.secret_len]
    }

    /// Whether `other` holds the same values, the sequence number aside.
    fn same_values(&self, other: &Record) -> bool {
        self.ssid() == other.ssid()
            && self.secret() == other.secret()
            && self.security == other.security
            && self.reconnection == other.reconnection
            && self.boot == other.boot
    }

    /// The slot's bytes.
    fn encode(&self) -> [u8; SLOT_LEN] {
        let mut slot = [0u8; SLOT_LEN];
        slot[..4].copy_from_slice(&MAGIC);
        slot[AT_VERSION] = VERSION;
        slot[AT_SECURITY] = self.security;
        slot[AT_NAME_LEN] = self.ssid_len as u8;
        slot[AT_SECRET_LEN] = self.secret_len as u8;
        slot[AT_SEQUENCE..AT_SEQUENCE + 4].copy_from_slice(&self.sequence.to_le_bytes());
        slot[AT_NAME..AT_NAME + SSID_MAX].copy_from_slice(&self.ssid);
        slot[AT_SECRET..AT_SECRET + SECRET_MAX].copy_from_slice(&self.secret);
        slot[AT_RECONNECTION] = self.reconnection as u8;
        slot[AT_BOOT] = self.boot as u8;
        let check = crc32(&slot[..AT_CHECK]);
        slot[AT_CHECK..].copy_from_slice(&check.to_le_bytes());
        slot
    }

    /// The record a slot holds, or `None` when it holds no undamaged one, or one with a value this
    /// reader cannot use.
    #[cfg(test)]
    fn decode(bytes: &[u8]) -> Option<Self> {
        Self::values_of(bytes, Self::sequence_of(bytes)?)
    }

    /// The sequence number of the copy a slot starts with, read from format 1's frame, which every
    /// format keeps; `None` when the slot starts with no undamaged frame: another magic, or a check
    /// that fails. The format is not checked here: a format this reader does not know is a newer
    /// version's copy, which [`Record::values_of`] cannot use.
    fn sequence_of(bytes: &[u8]) -> Option<u32> {
        let slot = bytes.get(..SLOT_LEN)?;
        if slot[..4] != MAGIC {
            return None;
        }
        let check = u32::from_le_bytes(slot[AT_CHECK..].try_into().ok()?);
        if crc32(&slot[..AT_CHECK]) != check {
            return None;
        }
        Some(u32::from_le_bytes(slot[AT_SEQUENCE..AT_SEQUENCE + 4].try_into().ok()?))
    }

    /// The record the undamaged slot `bytes` holds, with sequence number `sequence`, or `None` when
    /// a value in it is one this reader cannot use: a format other than 1, a security kind, a
    /// reconnection or a boot value outside the known set, a name of 0 or more than 32 bytes, or a
    /// secret of more than 128.
    fn values_of(bytes: &[u8], sequence: u32) -> Option<Self> {
        let slot = bytes.get(..SLOT_LEN)?;
        if slot[AT_VERSION] != VERSION {
            return None;
        }
        let ssid_len = usize::from(slot[AT_NAME_LEN]);
        let secret_len = usize::from(slot[AT_SECRET_LEN]);
        let reconnection = Reconnection::from_code(i32::from(slot[AT_RECONNECTION]))?;
        let boot = match slot[AT_BOOT] {
            0 => BootConnection::Background,
            1 => BootConnection::BeforeMain,
            2 => BootConnection::OnConnect,
            _ => return None,
        };
        let mut record = Record::new(
            slot.get(AT_NAME..AT_NAME + ssid_len)?,
            slot.get(AT_SECRET..AT_SECRET + secret_len)?,
            slot[AT_SECURITY],
            reconnection,
            boot,
        )?;
        record.sequence = sequence;
        Some(record)
    }
}

/// What a board's stored settings hold now.
#[derive(Debug)]
pub enum Held {
    /// No undamaged copy: no network is stored.
    Nothing,
    /// The current copy, in this slot, holds a record this reader can use.
    Record(usize, Record),
    /// The current copy, in this slot, is undamaged but holds a value this reader cannot use, which a
    /// newer version stored. No record is current -- never an older copy -- and the copy is kept as
    /// it is, for a reader that can use it.
    Unusable(usize),
}

impl Drop for Record {
    fn drop(&mut self) {
        self.secret.fill(0);
        core::hint::black_box(&self.secret);
    }
}

impl core::fmt::Debug for Record {
    /// The name's length and the secret's, never a byte of either.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Record")
            .field("ssid_len", &self.ssid_len)
            .field("secret_len", &self.secret_len)
            .field("security", &self.security)
            .field("reconnection", &self.reconnection)
            .field("boot", &self.boot)
            .field("sequence", &self.sequence)
            .finish()
    }
}

/// Whether `kinds` is a set a join can take: at least one kind, no bit outside the three, and no
/// secured kind beside an open one.
#[must_use]
pub fn kinds_valid(kinds: u8) -> bool {
    kinds != 0 && kinds & !security::ALL == 0 && (kinds == security::OPEN || kinds & security::OPEN == 0)
}

/// Whether slot `slot`'s whole unit reads erased.
fn erased(store: &impl RecordStore, slot: usize) -> bool {
    store.read(slot).iter().all(|&byte| byte == 0xFF)
}

/// Whether sequence number `a` is newer than `b`, across a wrap.
fn newer(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

/// Bytes that may hold a secret: wiped when dropped.
struct Wiped(Vec<u8>);

impl Drop for Wiped {
    fn drop(&mut self) {
        self.0.fill(0);
        core::hint::black_box(&self.0);
    }
}

/// The undamaged copy a slot holds: its sequence number, its record when this reader can use every
/// value in it, and the bytes of its later records.
struct Undamaged<'a> {
    sequence: u32,
    record: Option<Record>,
    later: &'a [u8],
}

/// The undamaged copy a slot's unit holds, or `None` when it holds none. Its frame must verify, and in
/// format 1 so must every later record, the list ending where the next magic reads erased or at the
/// unit's end. A copy in a format this reader does not know is judged by its frame alone: what follows
/// the frame is that format's, so it is neither read nor carried into a new copy.
fn parse(unit: &[u8]) -> Option<Undamaged<'_>> {
    let sequence = Record::sequence_of(unit)?;
    if unit.get(AT_VERSION) != Some(&VERSION) {
        return Some(Undamaged { sequence, record: None, later: &[] });
    }
    let mut at = SLOT_LEN;
    while let Some(magic) = unit.get(at..at + 4) {
        if magic == [0xFF; 4] {
            break;
        }
        if magic == MAGIC {
            return None;
        }
        let length = usize::from(u16::from_le_bytes(unit.get(at + 5..at + 7)?.try_into().ok()?));
        if length < LATER_MIN {
            return None;
        }
        let later = unit.get(at..at.checked_add(length)?)?;
        let (covered, check) = later.split_at(length - 4);
        if crc32(covered) != u32::from_le_bytes(check.try_into().ok()?) {
            return None;
        }
        at += length;
    }
    Some(Undamaged { sequence, record: Record::values_of(unit, sequence), later: &unit[SLOT_LEN..at] })
}

/// The current copy: its slot, its sequence number, its record when this reader can use it, and the
/// bytes of its later records.
struct Current {
    slot: usize,
    sequence: u32,
    record: Option<Record>,
    later: Wiped,
}

/// The newest undamaged copy, whether or not this reader can use its values, found without writing;
/// `None` when no slot holds an undamaged copy.
fn newest(store: &impl RecordStore) -> Option<Current> {
    let mut current: Option<Current> = None;
    for slot in 0..SLOTS {
        let Some(found) = parse(store.read(slot)) else { continue };
        if current.as_ref().is_none_or(|held| newer(found.sequence, held.sequence)) {
            current = Some(Current {
                slot,
                sequence: found.sequence,
                record: found.record,
                later: Wiped(found.later.to_vec()),
            });
        }
    }
    current
}

/// The current copy, or `None` when the storage holds no undamaged one. Every other slot that is not
/// erased -- a copy the current one replaced, or a damaged one, a write cut short -- is erased on the
/// way, so nothing it holds can be taken for a record later. The current copy is never erased, even
/// one whose values this reader cannot use.
fn current(store: &mut impl RecordStore) -> Option<Current> {
    let current = newest(store);
    for slot in 0..SLOTS {
        let is_current = current.as_ref().is_some_and(|held| held.slot == slot);
        if !is_current && !erased(store, slot) {
            let _ = store.erase(slot);
        }
    }
    current
}

/// What `current` found, as a caller reads it.
fn held_of(current: Option<Current>) -> Held {
    match current {
        None => Held::Nothing,
        Some(Current { slot, record: Some(record), .. }) => Held::Record(slot, record),
        Some(Current { slot, record: None, .. }) => Held::Unusable(slot),
    }
}

/// What the storage holds now. Every slot that is not erased and is not the current copy -- a copy
/// the current one replaced, or a damaged one -- is erased on the way, so nothing it holds can be
/// taken for a record later; the current copy is kept, even one this reader cannot use.
pub fn load_held(store: &mut impl RecordStore) -> Held {
    held_of(current(store))
}

/// The current record and its slot, or `None` when the storage holds none this reader can use, as
/// [`load_held`] finds it, with the same erases on the way.
pub fn load(store: &mut impl RecordStore) -> Option<(usize, Record)> {
    match load_held(store) {
        Held::Record(slot, record) => Some((slot, record)),
        Held::Nothing | Held::Unusable(_) => None,
    }
}

/// Writes `image` into slot `slot` so that a cut anywhere leaves the slot not valid: the unit is
/// erased first unless every byte of it reads erased, then the pages after the first are programmed
/// and read back from the lowest up, and the first page, which holds format 1, goes last. Each page
/// is programmed once. Returns whether every page was programmed and read back.
fn write_slot(store: &mut impl RecordStore, slot: usize, image: &[u8]) -> bool {
    if image.len() > store.read(slot).len() {
        return false;
    }
    if !erased(store, slot) {
        let wiped = store.erase(slot) && erased(store, slot);
        if !wiped {
            return false;
        }
    }
    let pages = image.len().div_ceil(PAGE);
    for page in (1..pages).chain(core::iter::once(0)) {
        let offset = page * PAGE;
        let end = image.len().min(offset + PAGE);
        let mut bytes = [0xFF; PAGE];
        bytes[..end - offset].copy_from_slice(&image[offset..end]);
        let landed = store.program(slot, offset, &bytes) && store.read(slot).get(offset..offset + PAGE) == Some(&bytes[..]);
        bytes.fill(0);
        core::hint::black_box(&bytes);
        if !landed {
            return false;
        }
    }
    true
}

/// Stores `record`, with every later record of the current slot copied forward after it, byte for
/// byte. The values stored before are erased once the new ones are written and read back; a write of
/// the values already stored writes nothing. The retired boot setting is stored as
/// [`BootConnection::Background`], so a write over a slot holding it always writes. A current copy
/// this reader cannot use is replaced as any other is: the new record takes the next sequence number.
pub fn store(store: &mut impl RecordStore, record: &Record) -> RecordWrite {
    let mut written = record.clone();
    written.boot = written.boot.in_effect();
    let current = current(store);
    if let Some(held) = &current
        && held.record.as_ref().is_some_and(|values| values.same_values(&written))
    {
        return RecordWrite::Unchanged;
    }
    let (target, sequence) = match &current {
        Some(held) => (SLOTS - 1 - held.slot, held.sequence.wrapping_add(1)),
        None => (0, 1),
    };
    let later: &[u8] = current.as_ref().map_or(&[], |held| &held.later.0);
    written.sequence = sequence;
    let mut first = written.encode();
    let mut image = Wiped(Vec::with_capacity(SLOT_LEN + later.len()));
    image.0.extend_from_slice(&first);
    first.fill(0);
    core::hint::black_box(&first);
    image.0.extend_from_slice(later);
    let read_back = write_slot(store, target, &image.0)
        && parse(store.read(target)).is_some_and(|back| {
            back.sequence == sequence
                && back.later == later
                && back.record.as_ref().is_some_and(|values| values.same_values(&written))
        });
    if !read_back {
        let _ = store.erase(target);
        return RecordWrite::Failed;
    }
    if let Some(held) = current
        && !store.erase(held.slot)
        && !store.erase(held.slot)
    {
        return RecordWrite::Failed;
    }
    RecordWrite::Written
}

/// Changes the boot setting of the stored record, as [`store`] stores it.
pub fn set_boot(store: &mut impl RecordStore, boot: BootConnection) -> RecordWrite {
    let Some((_, mut record)) = load(store) else {
        return RecordWrite::NoRecord;
    };
    record.boot = boot;
    self::store(store, &record)
}

/// Erases both slots. Returns whether both then read erased.
pub fn clear(store: &mut impl RecordStore) -> bool {
    let mut clear = true;
    for slot in 0..SLOTS {
        if !erased(store, slot) {
            clear &= store.erase(slot);
        }
        clear &= erased(store, slot);
    }
    clear
}

/// What the storage holds now, read without writing: a copy replaced or cut short is left as it is,
/// for the next write to erase. For a reader that must not change the board, such as a tool that
/// only shows what is stored.
pub fn peek_held(store: &impl RecordStore) -> Held {
    held_of(newest(store))
}

/// The current record, read without writing as [`peek_held`] reads it, or `None` when the storage
/// holds none this reader can use.
pub fn peek(store: &impl RecordStore) -> Option<Record> {
    match peek_held(store) {
        Held::Record(_, record) => Some(record),
        Held::Nothing | Held::Unusable(_) => None,
    }
}

/// A board's flash as a tool reaches it: reads, erases and page programs, at offsets from the start
/// of the flash region the settings lie in.
pub trait RegionFlash {
    /// The `len` bytes at `offset`.
    ///
    /// # Errors
    /// The flash could not be read; the text says why.
    fn read(&mut self, offset: u32, len: usize) -> Result<Vec<u8>, String>;

    /// Erases the `len` bytes at `offset`, whole erase units.
    ///
    /// # Errors
    /// The erase did not complete; the text says why.
    fn erase(&mut self, offset: u32, len: u32) -> Result<(), String>;

    /// Programs one page at `offset`, a page boundary, where the flash reads erased.
    ///
    /// # Errors
    /// The program did not complete; the text says why.
    fn program(&mut self, offset: u32, page: &[u8; PAGE]) -> Result<(), String>;
}

/// The two slots of a board's stored settings over a [`RegionFlash`]: slot `n` is the erase unit at
/// `offset + n * erase_unit`. It reads both units when it opens, and reads back what it erases and
/// programs, so what [`RecordStore::read`] answers is the flash as last read. The first error the
/// flash gives is kept, and every later step fails without reaching the flash.
pub struct RegionStore<'a, F: RegionFlash + ?Sized> {
    flash: &'a mut F,
    offset: u32,
    erase_unit: u32,
    units: [Vec<u8>; SLOTS],
    error: Option<String>,
}

impl<'a, F: RegionFlash + ?Sized> RegionStore<'a, F> {
    /// The store over the slots at `offset`, each `erase_unit` bytes, both read now.
    ///
    /// # Errors
    /// A unit that could not be read; the text says why.
    pub fn open(flash: &'a mut F, offset: u32, erase_unit: u32) -> Result<Self, String> {
        let unit = erase_unit as usize;
        let first = flash.read(offset, unit)?;
        let second = flash.read(offset + erase_unit, unit)?;
        Ok(RegionStore { flash, offset, erase_unit, units: [first, second], error: None })
    }

    /// The first error the flash gave, if any.
    #[must_use]
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Where slot `slot`'s unit starts in the flash region, `within` bytes into it.
    fn at(&self, slot: usize, within: usize) -> u32 {
        let slot = u32::try_from(slot).unwrap_or(u32::MAX);
        let within = u32::try_from(within).unwrap_or(u32::MAX);
        self.offset.saturating_add(slot.saturating_mul(self.erase_unit)).saturating_add(within)
    }

    /// Runs `step` on the flash unless an earlier error stopped the store, and keeps the first error
    /// it gives.
    fn step<T>(&mut self, step: impl FnOnce(&mut F) -> Result<T, String>) -> Option<T> {
        if self.error.is_some() {
            return None;
        }
        match step(&mut *self.flash) {
            Ok(value) => Some(value),
            Err(error) => {
                self.error = Some(error);
                None
            }
        }
    }
}

impl<F: RegionFlash + ?Sized> RecordStore for RegionStore<'_, F> {
    fn read(&self, slot: usize) -> &[u8] {
        &self.units[slot]
    }

    fn erase(&mut self, slot: usize) -> bool {
        let (at, unit) = (self.at(slot, 0), self.erase_unit);
        if self.step(|flash| flash.erase(at, unit)).is_none() {
            return false;
        }
        let Some(bytes) = self.step(|flash| flash.read(at, unit as usize)) else { return false };
        let erased = bytes.iter().all(|&byte| byte == 0xFF);
        self.units[slot] = bytes;
        erased
    }

    fn program(&mut self, slot: usize, offset: usize, page: &[u8; PAGE]) -> bool {
        let at = self.at(slot, offset);
        if self.step(|flash| flash.program(at, page)).is_none() {
            return false;
        }
        let Some(bytes) = self.step(|flash| flash.read(at, PAGE)) else { return false };
        match self.units[slot].get_mut(offset..offset + PAGE) {
            Some(cached) if bytes.len() == PAGE => {
                cached.copy_from_slice(&bytes);
                true
            }
            _ => {
                self.error = Some(String::from("the flash read back a page of another size"));
                false
            }
        }
    }
}

/// CRC-32 as IEEE 802.3 defines it: reflected, polynomial `0xEDB88320`, initial and final value all
/// ones.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    /// The bytes of one test erase unit: four pages, so a list of later records can cross pages.
    const UNIT: usize = 4 * PAGE;

    /// Two erase units in memory, counting what was asked of them, with a switch that makes the
    /// next program or erase fail.
    struct Flash {
        units: [[u8; UNIT]; SLOTS],
        programs: Vec<(usize, usize)>,
        erases: Vec<usize>,
        fail_program: bool,
        fail_erase: bool,
    }

    impl Flash {
        fn new() -> Self {
            Flash { units: [[0xFF; UNIT]; SLOTS], programs: Vec::new(), erases: Vec::new(), fail_program: false, fail_erase: false }
        }
    }

    impl RecordStore for Flash {
        fn read(&self, slot: usize) -> &[u8] {
            &self.units[slot]
        }

        fn erase(&mut self, slot: usize) -> bool {
            self.erases.push(slot);
            if self.fail_erase {
                return false;
            }
            self.units[slot] = [0xFF; UNIT];
            true
        }

        fn program(&mut self, slot: usize, offset: usize, page: &[u8; PAGE]) -> bool {
            self.programs.push((slot, offset));
            assert!(offset.is_multiple_of(PAGE) && offset + PAGE <= UNIT, "a page at a page boundary inside the unit");
            let at = &mut self.units[slot][offset..offset + PAGE];
            assert!(at.iter().all(|&b| b == 0xFF), "a page is programmed once, onto erased flash");
            if self.fail_program {
                at[..PAGE / 2].copy_from_slice(&page[..PAGE / 2]);
                return false;
            }
            at.copy_from_slice(page);
            true
        }
    }

    fn network(name: &[u8], secret: &[u8]) -> Record {
        Record::new(name, secret, security::WPA2 | security::WPA3, Reconnection::Automatic, BootConnection::Background).unwrap()
    }

    /// Whether `needle` appears anywhere in the flash.
    fn holds(flash: &Flash, needle: &[u8]) -> bool {
        flash.units.iter().any(|unit| unit.windows(needle.len()).any(|w| w == needle))
    }

    #[test]
    fn an_empty_store_holds_no_record_and_a_written_one_reads_back() {
        let mut flash = Flash::new();
        assert!(load(&mut flash).is_none());
        assert_eq!(store(&mut flash, &network(b"bench-net", b"secret-one")), RecordWrite::Written);
        let (slot, record) = load(&mut flash).expect("the record");
        assert_eq!((slot, record.ssid(), record.secret()), (0, &b"bench-net"[..], &b"secret-one"[..]));
        assert_eq!(record.security, security::WPA2 | security::WPA3);
    }

    /// THE HARD RULE: once a new record is written, the name and secret it replaced are gone from the
    /// storage -- not marked, not superseded, erased.
    #[test]
    fn a_replaced_name_and_secret_do_not_survive_the_write_that_replaces_them() {
        let mut flash = Flash::new();
        store(&mut flash, &network(b"old-network", b"old-secret-123"));
        assert!(holds(&flash, b"old-secret-123"));
        assert_eq!(store(&mut flash, &network(b"new-network", b"new-secret-456")), RecordWrite::Written);
        assert!(!holds(&flash, b"old-secret-123"), "the replaced secret is erased");
        assert!(!holds(&flash, b"old-network"), "the replaced name is erased");
        let (slot, record) = load(&mut flash).unwrap();
        assert_eq!((slot, record.ssid()), (1, &b"new-network"[..]), "the new record took the other slot");
        assert_eq!(flash.units[0], [0xFF; UNIT], "the old slot reads erased");
    }

    #[test]
    fn writing_the_values_already_stored_writes_nothing() {
        let mut flash = Flash::new();
        store(&mut flash, &network(b"bench-net", b"secret-one"));
        let (programs, erases) = (flash.programs.len(), flash.erases.len());
        assert_eq!(store(&mut flash, &network(b"bench-net", b"secret-one")), RecordWrite::Unchanged);
        assert_eq!((flash.programs.len(), flash.erases.len()), (programs, erases), "no wear for a repeat");
        assert_eq!(store(&mut flash, &network(b"bench-net", b"secret-two")), RecordWrite::Written, "a new secret is a change");
    }

    /// Power lost after the new slot is written and before the old one is erased: both slots are
    /// valid, the higher sequence wins, and the next read erases the loser.
    #[test]
    fn two_valid_slots_resolve_to_the_newer_and_the_older_is_erased_on_read() {
        let mut flash = Flash::new();
        store(&mut flash, &network(b"first", b"secret-one"));
        let first = flash.units[0];
        store(&mut flash, &network(b"second", b"secret-two"));
        flash.units[0] = first;
        let (slot, record) = load(&mut flash).unwrap();
        assert_eq!((slot, record.ssid()), (1, &b"second"[..]));
        assert_eq!(flash.units[0], [0xFF; UNIT], "the superseded slot was erased by the read");
        assert!(!holds(&flash, b"secret-one"));
    }

    /// A write cut short leaves bytes that are not a record but may hold part of a secret; the write
    /// reports failure, erases what landed, and the record stays as it was.
    #[test]
    fn a_failed_write_erases_what_landed_and_leaves_the_record_as_it_was() {
        let mut flash = Flash::new();
        store(&mut flash, &network(b"kept", b"kept-secret"));
        flash.fail_program = true;
        assert_eq!(store(&mut flash, &network(b"lost", b"lost-secret-xyz")), RecordWrite::Failed);
        flash.fail_program = false;
        assert_eq!(flash.units[1], [0xFF; UNIT], "the torn slot was erased");
        assert_eq!(load(&mut flash).unwrap().1.ssid(), b"kept");
    }

    /// A torn slot found on a read is erased too: its bytes are not a record worth trying, and they
    /// may hold part of a secret.
    #[test]
    fn a_slot_that_is_neither_erased_nor_a_record_is_erased_on_read() {
        let mut flash = Flash::new();
        flash.units[1][..8].copy_from_slice(b"LWFRgarb");
        assert!(load(&mut flash).is_none());
        assert_eq!(flash.units[1], [0xFF; UNIT]);
    }

    #[test]
    fn a_zeroed_slot_and_a_bad_checksum_read_as_no_record() {
        let mut flash = Flash::new();
        store(&mut flash, &network(b"bench-net", b"secret-one"));
        flash.units[0][AT_NAME] ^= 1;
        assert!(Record::decode(&flash.units[0]).is_none(), "one flipped bit fails the check");
        assert!(Record::decode(&[0u8; PAGE]).is_none(), "a zeroed slot is empty");
        assert!(Record::decode(&[0xFFu8; PAGE]).is_none(), "an erased slot is empty");
    }

    #[test]
    fn clear_erases_both_slots() {
        let mut flash = Flash::new();
        store(&mut flash, &network(b"first", b"secret-one"));
        store(&mut flash, &network(b"second", b"secret-two"));
        assert!(clear(&mut flash));
        assert!(load(&mut flash).is_none());
        assert_eq!(flash.units, [[0xFF; UNIT]; SLOTS]);
    }

    #[test]
    fn the_boot_setting_is_rewritten_with_the_rest_kept_and_the_old_slot_erased() {
        let mut flash = Flash::new();
        assert_eq!(set_boot(&mut flash, BootConnection::OnConnect), RecordWrite::NoRecord);
        store(&mut flash, &network(b"bench-net", b"secret-one"));
        assert_eq!(set_boot(&mut flash, BootConnection::Background), RecordWrite::Unchanged);
        assert_eq!(set_boot(&mut flash, BootConnection::OnConnect), RecordWrite::Written);
        let (slot, record) = load(&mut flash).unwrap();
        assert_eq!((slot, record.boot, record.secret()), (1, BootConnection::OnConnect, &b"secret-one"[..]));
        assert_eq!(flash.units[0], [0xFF; UNIT]);
    }

    /// Rewrites slot `slot`'s byte `at` as `value` and makes its check again, so the slot is undamaged
    /// and holds that value.
    fn with_byte(flash: &mut Flash, slot: usize, at: usize, value: u8) {
        flash.units[slot][at] = value;
        let check = crc32(&flash.units[slot][..AT_CHECK]);
        flash.units[slot][AT_CHECK..SLOT_LEN].copy_from_slice(&check.to_le_bytes());
    }

    /// Rewrites slot `slot`'s boot byte as `code` and makes its check again, so the slot verifies.
    fn with_boot_code(flash: &mut Flash, slot: usize, code: u8) {
        with_byte(flash, slot, AT_BOOT, code);
    }

    /// Two undamaged copies, as a power cut between a write and the erase after it leaves them:
    /// "first" with sequence 1 in slot 0, and "second" with sequence 2 in slot 1.
    fn two_copies() -> Flash {
        let mut flash = Flash::new();
        store(&mut flash, &network(b"first", b"secret-one"));
        let first = flash.units[0];
        store(&mut flash, &network(b"second", b"secret-two"));
        flash.units[0] = first;
        flash
    }

    /// Each value this reader cannot use, at its offset: a format other than 1, kinds outside the
    /// known set or open beside a secured kind, reconnection 2, boot 3, a name of 0 or 33 bytes, and a
    /// secret of 129 bytes.
    const UNUSABLE: [(&str, usize, u8); 12] = [
        ("format 0", AT_VERSION, 0),
        ("format 2", AT_VERSION, 2),
        ("format 255", AT_VERSION, 0xFF),
        ("kinds 0", AT_SECURITY, 0),
        ("kinds 8", AT_SECURITY, 8),
        ("open beside WPA2", AT_SECURITY, 3),
        ("open beside WPA2 and WPA3", AT_SECURITY, 7),
        ("reconnection 2", AT_RECONNECTION, 2),
        ("boot 3", AT_BOOT, 3),
        ("a name of 0 bytes", AT_NAME_LEN, 0),
        ("a name of 33 bytes", AT_NAME_LEN, 33),
        ("a secret of 129 bytes", AT_SECRET_LEN, 129),
    ];

    /// **THE NEWEST UNDAMAGED COPY IS CURRENT**, whether or not this reader can use every value in
    /// it: a newer version may store a value this one does not know. Then no record is current --
    /// never the older copy -- the copy is kept as it is for a reader that can use it, and the older
    /// copy is erased as replaced. A reader that writes nothing finds the same.
    #[test]
    fn the_newest_undamaged_copy_is_current_even_when_this_reader_cannot_use_a_value_in_it() {
        for (what, at, value) in UNUSABLE {
            let mut flash = two_copies();
            with_byte(&mut flash, 1, at, value);
            let newer = flash.units[1];
            assert!(matches!(peek_held(&flash), Held::Unusable(1)), "{what}: the newer copy is current");
            assert!(peek(&flash).is_none(), "{what}: a reader that writes nothing takes no record, never the older one");
            assert!(load(&mut flash).is_none(), "{what}: no record is current, never the older one");
            assert_eq!(flash.units[1], newer, "{what}: the newer copy is kept as it is");
            assert_eq!(flash.units[0], [0xFF; UNIT], "{what}: the older copy is erased as replaced");
            assert!(matches!(load_held(&mut flash), Held::Unusable(1)), "{what}: and stays current");
        }
        assert!(matches!(peek_held(&Flash::new()), Held::Nothing));
        let mut flash = two_copies();
        assert!(matches!(load_held(&mut flash), Held::Record(1, ref record) if record.ssid() == b"second"));
    }

    /// **A DAMAGED COPY IS A WRITE CUT SHORT**: its check fails, so the older undamaged copy is
    /// current, and the damaged one is erased, since it may hold part of a secret.
    #[test]
    fn a_newer_copy_whose_check_fails_is_damaged_and_the_older_copy_is_current() {
        let mut flash = two_copies();
        flash.units[1][AT_SECURITY] ^= 0x04;
        assert_eq!(peek(&flash).expect("the older copy").ssid(), b"first");
        let (slot, record) = load(&mut flash).expect("the older copy");
        assert_eq!((slot, record.ssid()), (0, &b"first"[..]));
        assert_eq!(flash.units[1], [0xFF; UNIT], "the damaged copy is erased");
    }

    /// **EVERY FORMAT KEEPS FORMAT 1's FRAME**, so a copy in a format this reader does not know is
    /// still checked by its frame's CRC-32 and ordered by its sequence number, and judged by that frame
    /// alone: what follows the frame is its own format's, never walked as format 1's later records and
    /// never carried into a new copy. An undamaged one is a newer version's copy: current when its
    /// number is higher, kept as it is, the older copy erased. One whose frame check fails is damaged.
    #[test]
    fn a_copy_in_another_format_is_judged_by_format_1s_frame_alone() {
        let mut flash = two_copies();
        let later = later_record(b"LTST", 1, b"a newer format's record");
        after_format_1(&mut flash, 1, &later);
        with_byte(&mut flash, 1, AT_VERSION, 2);
        let newer = flash.units[1];
        assert!(matches!(peek_held(&flash), Held::Unusable(1)), "the newer copy in format 2 is current");
        assert!(matches!(load_held(&mut flash), Held::Unusable(1)), "and stays current");
        assert_eq!(flash.units[1], newer, "it is kept as it is");
        assert_eq!(flash.units[0], [0xFF; UNIT], "the older copy is erased as replaced");
        assert_eq!(store(&mut flash, &network(b"mine", b"secret-three")), RecordWrite::Written);
        let (slot, record) = load(&mut flash).expect("the new record");
        assert_eq!((slot, record.ssid(), record.sequence), (0, &b"mine"[..], 3), "the other slot, the next sequence number");
        assert!(flash.units[0][SLOT_LEN..].iter().all(|&byte| byte == 0xFF), "nothing of the format 2 copy is carried");
        assert_eq!(flash.units[1], [0xFF; UNIT], "the format 2 copy it replaced is erased");

        let mut flash = two_copies();
        with_byte(&mut flash, 1, AT_VERSION, 2);
        flash.units[1][AT_NAME] ^= 0x01;
        let (slot, record) = load(&mut flash).expect("the older copy");
        assert_eq!((slot, record.ssid()), (0, &b"first"[..]), "a format 2 copy whose frame check fails is damaged");
        assert_eq!(flash.units[1], [0xFF; UNIT], "and erased");

        let mut broken = later_record(b"LTST", 1, b"cut short");
        let last = broken.len() - 1;
        broken[last] ^= 0x01;
        for (after, what) in [(&[0u8; 16][..], "bytes that are no list of later records"), (&broken[..], "a record that fails format 1's check")] {
            let mut flash = two_copies();
            after_format_1(&mut flash, 1, after);
            with_byte(&mut flash, 1, AT_VERSION, 2);
            let newer = flash.units[1];
            assert!(matches!(load_held(&mut flash), Held::Unusable(1)), "a format 2 copy followed by {what} is current");
            assert!(load(&mut flash).is_none(), "{what}: no stored network, never the older copy");
            assert_eq!(flash.units[1], newer, "{what}: the format 2 copy is kept as it is");
        }
    }

    /// Storing over a copy this reader cannot use is a write like any other: the new record takes the
    /// other slot and the next sequence number, the later records are copied forward, and the copy it
    /// replaces is erased. A changed boot setting finds no record to change, and keeps the copy.
    #[test]
    fn a_network_stored_over_an_unusable_copy_takes_the_next_sequence_number_and_replaces_it() {
        let mut flash = Flash::new();
        store(&mut flash, &network(b"newer", b"secret-one"));
        let later = later_record(b"LTST", 1, b"a newer writer's record");
        after_format_1(&mut flash, 0, &later);
        with_byte(&mut flash, 0, AT_RECONNECTION, 2);
        let kept = flash.units[0];
        assert_eq!(set_boot(&mut flash, BootConnection::OnConnect), RecordWrite::NoRecord, "no record this reader can change");
        assert_eq!(flash.units[0], kept, "and the copy is kept");
        assert_eq!(store(&mut flash, &network(b"mine", b"secret-two")), RecordWrite::Written);
        let (slot, record) = load(&mut flash).expect("the new record");
        assert_eq!((slot, record.ssid(), record.sequence), (1, &b"mine"[..], 2), "the other slot, the next sequence number");
        assert_eq!(flash.units[0], [0xFF; UNIT], "the copy it replaced is erased");
        assert_eq!(&flash.units[1][SLOT_LEN..SLOT_LEN + later.len()], &later[..], "its later records are copied forward");
    }

    /// The boot byte of the slot that holds the current record.
    fn current_boot_byte(flash: &mut Flash) -> u8 {
        let (slot, _) = load(flash).expect("a record");
        flash.units[slot][AT_BOOT]
    }

    /// A slot whose boot byte is 1, the retired "join before the program starts", is a valid slot: it
    /// reads as that value, for the board to follow as background. A byte past 2 names nothing.
    #[test]
    fn a_slot_holding_the_retired_boot_value_is_valid_and_reads_as_that_value() {
        let mut flash = Flash::new();
        store(&mut flash, &network(b"bench-net", b"secret-one"));
        with_boot_code(&mut flash, 0, 1);
        assert_eq!(peek(&flash).expect("still a record").boot, BootConnection::BeforeMain);
        let (slot, record) = load(&mut flash).expect("still a record");
        assert_eq!((slot, record.boot, record.ssid()), (0, BootConnection::BeforeMain, &b"bench-net"[..]));
        with_boot_code(&mut flash, 0, 3);
        assert!(load(&mut flash).is_none(), "a boot byte of 3 names no setting this reader knows, so no record is current");
    }

    /// Every write stores 0 where the record held the retired 1: a network stored over it with its
    /// settings carried, a boot setting changed or set again, and a record made with the retired
    /// value.
    #[test]
    fn every_write_stores_the_retired_boot_value_as_background() {
        let carried = Record::new(b"bench-net", b"secret-one", security::WPA2 | security::WPA3, Reconnection::Automatic, BootConnection::BeforeMain).unwrap();
        for (what, write, byte) in [
            ("the same network, its settings carried", 0, 0),
            ("set_boot(Background)", 1, 0),
            ("set_boot(BeforeMain)", 2, 0),
            ("set_boot(OnConnect)", 3, 2),
        ] {
            let mut flash = Flash::new();
            store(&mut flash, &network(b"bench-net", b"secret-one"));
            with_boot_code(&mut flash, 0, 1);
            let outcome = match write {
                0 => store(&mut flash, &carried),
                1 => set_boot(&mut flash, BootConnection::Background),
                2 => set_boot(&mut flash, BootConnection::BeforeMain),
                _ => set_boot(&mut flash, BootConnection::OnConnect),
            };
            assert_eq!(outcome, RecordWrite::Written, "{what} writes over a held 1");
            assert_eq!(current_boot_byte(&mut flash), byte, "{what}");
            assert_eq!(load(&mut flash).unwrap().1.secret(), b"secret-one", "{what} keeps the rest");
        }
        let mut flash = Flash::new();
        assert_eq!(store(&mut flash, &carried), RecordWrite::Written);
        assert_eq!(current_boot_byte(&mut flash), 0, "a record made with the retired value is stored as background");
        assert_eq!(set_boot(&mut flash, BootConnection::BeforeMain), RecordWrite::Unchanged, "and setting it again writes nothing");
    }

    #[test]
    fn the_sequence_survives_a_wrap() {
        assert!(newer(0, u32::MAX));
        assert!(newer(5, 4));
        assert!(!newer(4, 5));
    }

    #[test]
    fn a_set_of_kinds_must_name_one_and_never_mix_open_with_a_secured_kind() {
        assert!(!kinds_valid(0));
        assert!(!kinds_valid(security::OPEN | security::WPA2));
        assert!(!kinds_valid(8));
        assert!(kinds_valid(security::OPEN));
        assert!(kinds_valid(security::WPA2 | security::WPA3));
    }

    /// The check is the CRC-32 every implementation agrees on: the standard check value.
    #[test]
    fn the_checksum_is_ieee_crc32() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn printing_a_record_prints_lengths_and_never_the_bytes() {
        let printed = std::format!("{:?}", network(b"bench-net", b"secret-one"));
        assert!(!printed.contains("bench") && !printed.contains("secret-one"), "{printed}");
        assert!(printed.contains("ssid_len: 9") && printed.contains("secret_len: 10"), "{printed}");
    }

    /// A later record, as a newer writer would leave it after format 1: its magic, its format, its
    /// whole length, its body, and the CRC-32 of every byte before the CRC.
    fn later_record(magic: &[u8; 4], format: u8, body: &[u8]) -> Vec<u8> {
        let length = 4 + 1 + 2 + body.len() + 4;
        let mut bytes = Vec::with_capacity(length);
        bytes.extend_from_slice(magic);
        bytes.push(format);
        bytes.extend_from_slice(&u16::try_from(length).unwrap().to_le_bytes());
        bytes.extend_from_slice(body);
        let check = crc32(&bytes);
        bytes.extend_from_slice(&check.to_le_bytes());
        bytes
    }

    /// Puts `bytes` right after format 1 in slot `slot`, as a newer writer would have.
    fn after_format_1(flash: &mut Flash, slot: usize, bytes: &[u8]) {
        flash.units[slot][SLOT_LEN..SLOT_LEN + bytes.len()].copy_from_slice(bytes);
    }

    /// THE COPY-FORWARD RULE: a writer carries every later record it does not know into the new slot,
    /// byte for byte, right after format 1, so a later record survives a writer that predates it.
    #[test]
    fn a_later_record_the_writer_does_not_know_is_copied_forward_byte_for_byte() {
        let mut flash = Flash::new();
        store(&mut flash, &network(b"first", b"secret-one"));
        let later = later_record(b"LTST", 1, b"a kind of record this writer does not know");
        after_format_1(&mut flash, 0, &later);
        assert_eq!(store(&mut flash, &network(b"second", b"secret-two")), RecordWrite::Written);
        let (slot, record) = load(&mut flash).unwrap();
        assert_eq!((slot, record.ssid()), (1, &b"second"[..]));
        assert_eq!(&flash.units[1][SLOT_LEN..SLOT_LEN + later.len()], &later[..], "the later record follows format 1");
        assert!(flash.units[1][SLOT_LEN + later.len()..].iter().all(|&b| b == 0xFF), "and erased bytes end the list");
        assert_eq!(flash.units[0], [0xFF; UNIT], "the old slot reads erased");
    }

    /// A slot is valid only when every record in it verifies: a later record that fails its check
    /// is a write cut short, so the slot is never current, and it is erased when found.
    #[test]
    fn a_slot_whose_later_record_fails_its_check_is_not_valid_and_is_erased_on_read() {
        let mut flash = Flash::new();
        store(&mut flash, &network(b"bench-net", b"secret-one"));
        let mut later = later_record(b"LTST", 1, b"cut short");
        let last = later.len() - 1;
        later[last] ^= 0x01;
        after_format_1(&mut flash, 0, &later);
        assert!(load(&mut flash).is_none(), "format 1 verifies on its own, the slot does not");
        assert_eq!(flash.units[0], [0xFF; UNIT], "the slot was erased: it may hold part of a secret");
    }

    /// Power lost after a newer writer's slot was half written: its later record fails, so the older
    /// slot is the current one, however high the newer one's sequence number.
    #[test]
    fn a_newer_slot_with_a_broken_later_record_loses_to_the_older_valid_slot() {
        let mut flash = Flash::new();
        store(&mut flash, &network(b"first", b"secret-one"));
        let first = flash.units[0];
        store(&mut flash, &network(b"second", b"secret-two"));
        flash.units[0] = first;
        let mut later = later_record(b"LTST", 1, b"cut short");
        let last = later.len() - 1;
        later[last] ^= 0x01;
        after_format_1(&mut flash, 1, &later);
        let (slot, record) = load(&mut flash).unwrap();
        assert_eq!((slot, record.ssid()), (0, &b"first"[..]));
        assert_eq!(flash.units[1], [0xFF; UNIT], "the broken slot was erased");
    }

    /// The list's bounds: a later record's length counts the whole record, 11 bytes or more, and stays
    /// inside the unit; and no later record takes format 1's magic. Any of these broken makes the slot
    /// invalid.
    #[test]
    fn a_later_record_too_short_past_the_unit_or_with_format_1s_magic_makes_the_slot_invalid() {
        let well_formed = later_record(b"LTST", 1, b"body");
        let mut too_short = well_formed.clone();
        too_short[5..7].copy_from_slice(&10u16.to_le_bytes());
        let mut past_the_unit = well_formed.clone();
        past_the_unit[5..7].copy_from_slice(&u16::try_from(UNIT - SLOT_LEN + 1).unwrap().to_le_bytes());
        let first_magic = later_record(b"LWFR", 1, b"body");
        for (what, bytes) in [("too short", too_short), ("past the unit", past_the_unit), ("format 1's magic", first_magic)] {
            let mut flash = Flash::new();
            store(&mut flash, &network(b"bench-net", b"secret-one"));
            after_format_1(&mut flash, 0, &bytes);
            assert!(load(&mut flash).is_none(), "{what}: the slot is not valid");
        }
    }

    /// The list may run to the unit's last byte: the unit's end ends it as erased bytes do.
    #[test]
    fn a_list_that_fills_the_unit_to_its_last_byte_is_valid() {
        let mut flash = Flash::new();
        store(&mut flash, &network(b"bench-net", b"secret-one"));
        let later = later_record(b"LTST", 1, &[0x5A; UNIT - SLOT_LEN - 11]);
        assert_eq!(SLOT_LEN + later.len(), UNIT);
        after_format_1(&mut flash, 0, &later);
        assert_eq!(load(&mut flash).unwrap().1.ssid(), b"bench-net");
    }

    /// THE WRITER's ORDER: the flash takes whole pages, so the pages after the first are programmed
    /// first, from the lowest up, and the first page, which holds format 1, last -- each page once.
    /// A cut anywhere before the last program leaves no valid format 1 in the new slot.
    #[test]
    fn the_pages_after_the_first_are_programmed_first_and_the_first_page_last_each_once() {
        let mut flash = Flash::new();
        store(&mut flash, &network(b"first", b"secret-one"));
        let later = later_record(b"LTST", 1, &[0x33; 400]);
        after_format_1(&mut flash, 0, &later);
        flash.programs.clear();
        assert_eq!(store(&mut flash, &network(b"second", b"secret-two")), RecordWrite::Written);
        assert_eq!(flash.programs, [(1, PAGE), (1, 2 * PAGE), (1, 0)]);
        assert_eq!(&flash.units[1][SLOT_LEN..SLOT_LEN + later.len()], &later[..], "the later record crosses pages intact");
    }

    /// A cut after the later pages and before the first page leaves bytes past an erased first page.
    /// The slot is neither erased nor valid, so the next read erases it.
    #[test]
    fn a_slot_with_bytes_past_an_erased_first_page_is_erased_on_read() {
        let mut flash = Flash::new();
        store(&mut flash, &network(b"kept", b"kept-secret"));
        flash.units[1][PAGE..PAGE + 4].copy_from_slice(b"LTST");
        let (slot, record) = load(&mut flash).unwrap();
        assert_eq!((slot, record.ssid()), (0, &b"kept"[..]));
        assert_eq!(flash.units[1], [0xFF; UNIT], "the bytes past the first page were erased");
    }

    /// A change of the boot setting is a write like any other: it copies the later records forward.
    #[test]
    fn a_changed_boot_setting_copies_the_later_records_forward() {
        let mut flash = Flash::new();
        store(&mut flash, &network(b"bench-net", b"secret-one"));
        let later = later_record(b"LTST", 1, b"kept across the change");
        after_format_1(&mut flash, 0, &later);
        assert_eq!(set_boot(&mut flash, BootConnection::OnConnect), RecordWrite::Written);
        let (slot, record) = load(&mut flash).unwrap();
        assert_eq!((slot, record.boot), (1, BootConnection::OnConnect));
        assert_eq!(&flash.units[1][SLOT_LEN..SLOT_LEN + later.len()], &later[..]);
    }

    /// A read that must not change the board: the newer of two valid slots is current, and the older
    /// one stays where a power cut left it, for the next write to erase.
    #[test]
    fn peek_reads_the_current_record_and_writes_nothing() {
        let mut flash = Flash::new();
        store(&mut flash, &network(b"first", b"secret-one"));
        let first = flash.units[0];
        store(&mut flash, &network(b"second", b"secret-two"));
        flash.units[0] = first;
        let (programs, erases) = (flash.programs.len(), flash.erases.len());
        assert_eq!(peek(&flash).expect("the current record").ssid(), b"second");
        assert_eq!((flash.programs.len(), flash.erases.len()), (programs, erases), "nothing was written");
        assert_eq!(flash.units[0], first, "the older slot is left for the next write");
        let mut broken = later_record(b"LTST", 1, b"cut short");
        let last = broken.len() - 1;
        broken[last] ^= 0x01;
        after_format_1(&mut flash, 1, &broken);
        assert_eq!(peek(&flash).expect("the older record").ssid(), b"first", "a slot cut short is never current");
        assert!(peek(&Flash::new()).is_none());
    }

    /// A region of flash in memory, its bytes outside the settings' place filled so a stray write
    /// shows, with a switch that makes the next program fail.
    struct Region {
        bytes: Vec<u8>,
        fail_program: bool,
        steps: Vec<String>,
    }

    impl RegionFlash for Region {
        fn read(&mut self, offset: u32, len: usize) -> Result<Vec<u8>, String> {
            let at = offset as usize;
            self.bytes.get(at..at + len).map(<[u8]>::to_vec).ok_or_else(|| std::format!("read past the region at {at:#x}"))
        }

        fn erase(&mut self, offset: u32, len: u32) -> Result<(), String> {
            self.steps.push(std::format!("erase {offset:#x} {len:#x}"));
            let at = offset as usize;
            self.bytes[at..at + len as usize].fill(0xFF);
            Ok(())
        }

        fn program(&mut self, offset: u32, page: &[u8; PAGE]) -> Result<(), String> {
            self.steps.push(std::format!("program {offset:#x}"));
            if self.fail_program {
                return Err(String::from("the probe stopped answering"));
            }
            let at = offset as usize;
            assert!(self.bytes[at..at + PAGE].iter().all(|&b| b == 0xFF), "a page is programmed onto erased flash");
            self.bytes[at..at + PAGE].copy_from_slice(page);
            Ok(())
        }
    }

    /// The slots lie where the board's facts put them: slot 0 at the offset, slot 1 one erase unit
    /// further, and no byte outside them changes.
    #[test]
    fn a_region_store_writes_the_slots_at_the_place_the_board_states() {
        let (offset, unit) = (0x800_u32, 0x400_u32);
        let mut region = Region { bytes: std::vec![0xA5; 0x1000], fail_program: false, steps: Vec::new() };
        region.bytes[0x800..0x1000].fill(0xFF);
        {
            let mut slots = RegionStore::open(&mut region, offset, unit).expect("opens");
            assert_eq!(store(&mut slots, &network(b"first", b"secret-one")), RecordWrite::Written);
            assert_eq!(store(&mut slots, &network(b"second", b"secret-two")), RecordWrite::Written);
            assert_eq!(load(&mut slots).expect("the record").1.ssid(), b"second");
            assert!(slots.error().is_none());
        }
        assert_eq!(&region.bytes[0xC00..0xC04], b"LWFR", "the second write is in slot 1, one erase unit on");
        assert!(region.bytes[0x800..0xC00].iter().all(|&b| b == 0xFF), "slot 0 was erased after it");
        assert!(region.bytes[..0x800].iter().all(|&b| b == 0xA5), "nothing before the place changed");
        assert!(region.steps.iter().any(|step| step == "erase 0x800 0x400"), "{:?}", region.steps);
    }

    /// A flash that stops answering stops the write: the store reports the first error, the record
    /// is as it was, and no later step reaches the flash.
    #[test]
    fn a_region_store_stops_at_the_first_flash_error() {
        let mut region = Region { bytes: std::vec![0xFF; 0x800], fail_program: false, steps: Vec::new() };
        {
            let mut slots = RegionStore::open(&mut region, 0, 0x400).expect("opens");
            assert_eq!(store(&mut slots, &network(b"kept", b"kept-secret")), RecordWrite::Written);
        }
        region.fail_program = true;
        let before = region.steps.len();
        let mut slots = RegionStore::open(&mut region, 0, 0x400).expect("opens");
        assert_eq!(store(&mut slots, &network(b"lost", b"lost-secret")), RecordWrite::Failed);
        assert_eq!(slots.error(), Some("the probe stopped answering"));
        assert!(!slots.erase(0), "a step after the error does not reach the flash");
        drop(slots);
        assert_eq!(region.steps.len(), before + 1, "one program was tried, and nothing after it: {:?}", region.steps);
    }

    /// A tool that chooses the route to a board as it runs holds the board's flash as a trait
    /// object, and the slots are kept over it as over any other flash.
    #[test]
    fn a_region_store_keeps_the_slots_over_a_flash_held_as_a_trait_object() {
        let mut region = Region { bytes: std::vec![0xFF; 0x800], fail_program: false, steps: Vec::new() };
        let flash: &mut dyn RegionFlash = &mut region;
        let mut slots = RegionStore::open(flash, 0, 0x400).expect("opens");
        assert_eq!(store(&mut slots, &network(b"held", b"held-secret")), RecordWrite::Written);
        assert_eq!(peek(&slots).expect("the record").ssid(), b"held");
    }
}
