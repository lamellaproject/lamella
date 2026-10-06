//! The network stored on the board: one record in two slots, each slot its own erase unit.
//!
//! A new record is written into the slot that does not hold the current one, read back, and only
//! then is the other slot ERASED -- so the name and secret a record replaces never survive it, on any
//! flash part, without programming a written location twice. Power lost between the two steps leaves
//! two valid slots; the higher sequence number wins, and the next read erases the other. A slot that
//! is neither erased nor a valid record -- a write cut short -- is erased when found, since its bytes
//! may hold part of a secret.
//!
//! A slot is 192 bytes at the start of its erase unit, every number little-endian:
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
//! | 173 | 1 | boot: 0 join in the background, 1 join before the program starts, 2 join on request |
//! | 174 | 14 | reserved, zero |
//! | 188 | 4 | CRC-32 (IEEE 802.3) of the 188 bytes before it |
//!
//! An erased slot reads `0xFF` and a zeroed one `0x00`, and neither carries the magic, so both read
//! as empty.

use lamella_net_core::wifi::{security, BootConnection, Reconnection, RecordWrite, SECRET_MAX, SSID_MAX};

/// The bytes one slot holds.
pub const SLOT_LEN: usize = 192;

/// The bytes of a slot a check reads: the first page of its erase unit, which is all a write ever
/// programs. The rest of the unit stays erased.
pub const SLOT_SPAN: usize = 256;

/// The slots: two.
pub const SLOTS: usize = 2;

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

/// The storage under the record: two erase units, each read through its first [`SLOT_SPAN`] bytes.
pub trait RecordStore {
    /// The first [`SLOT_SPAN`] bytes of slot `slot` (0 or 1), as the storage holds them now.
    fn read(&self, slot: usize) -> &[u8];

    /// Erases slot `slot`'s whole unit. Returns whether it then reads erased.
    fn erase(&mut self, slot: usize) -> bool;

    /// Programs `bytes` (at most [`SLOT_SPAN`]) at the start of slot `slot`, which reads erased.
    /// Returns whether the storage took the write.
    fn program(&mut self, slot: usize, bytes: &[u8]) -> bool;
}

/// A store behind a box, for a holder that names the store's type only when it is built.
impl<T: RecordStore + ?Sized> RecordStore for alloc::boxed::Box<T> {
    fn read(&self, slot: usize) -> &[u8] {
        (**self).read(slot)
    }

    fn erase(&mut self, slot: usize) -> bool {
        (**self).erase(slot)
    }

    fn program(&mut self, slot: usize, bytes: &[u8]) -> bool {
        (**self).program(slot, bytes)
    }
}

/// A store with nothing in it that takes no write: a board that keeps no settings holds no network.
pub struct NoStore;

impl RecordStore for NoStore {
    fn read(&self, _slot: usize) -> &[u8] {
        &[0xFF; SLOT_SPAN]
    }

    fn erase(&mut self, _slot: usize) -> bool {
        true
    }

    fn program(&mut self, _slot: usize, _bytes: &[u8]) -> bool {
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

    /// The record a slot holds, or `None` when it holds no valid one.
    fn decode(bytes: &[u8]) -> Option<Self> {
        let slot = bytes.get(..SLOT_LEN)?;
        if slot[..4] != MAGIC || slot[AT_VERSION] != VERSION {
            return None;
        }
        let check = u32::from_le_bytes(slot[AT_CHECK..].try_into().ok()?);
        if crc32(&slot[..AT_CHECK]) != check {
            return None;
        }
        let ssid_len = usize::from(slot[AT_NAME_LEN]);
        let secret_len = usize::from(slot[AT_SECRET_LEN]);
        let reconnection = Reconnection::from_code(i32::from(slot[AT_RECONNECTION]))?;
        let boot = BootConnection::from_code(i32::from(slot[AT_BOOT]))?;
        let mut record = Record::new(
            slot.get(AT_NAME..AT_NAME + ssid_len)?,
            slot.get(AT_SECRET..AT_SECRET + secret_len)?,
            slot[AT_SECURITY],
            reconnection,
            boot,
        )?;
        record.sequence = u32::from_le_bytes(slot[AT_SEQUENCE..AT_SEQUENCE + 4].try_into().ok()?);
        Some(record)
    }
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

/// Whether slot `slot` reads erased.
fn erased(store: &impl RecordStore, slot: usize) -> bool {
    store.read(slot).iter().all(|&byte| byte == 0xFF)
}

/// Whether sequence number `a` is newer than `b`, across a wrap.
fn newer(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

/// The current record and its slot, or `None` when the storage holds none. Every other slot that is
/// not erased -- a record the current one replaced, or a write cut short -- is erased on the way, so
/// nothing it holds can be taken for a record later.
pub fn load(store: &mut impl RecordStore) -> Option<(usize, Record)> {
    let mut current: Option<(usize, Record)> = None;
    for slot in 0..SLOTS {
        let Some(record) = Record::decode(store.read(slot)) else { continue };
        let replaces = match &current {
            None => true,
            Some((_, held)) => newer(record.sequence, held.sequence),
        };
        if replaces {
            current = Some((slot, record));
        }
    }
    for slot in 0..SLOTS {
        let is_current = current.as_ref().is_some_and(|(at, _)| *at == slot);
        if !is_current && !erased(store, slot) {
            let _ = store.erase(slot);
        }
    }
    current
}

/// Stores `record`. The values stored before are erased once the new ones are written and read back;
/// a write of the values already stored writes nothing.
pub fn store(store: &mut impl RecordStore, record: &Record) -> RecordWrite {
    let current = load(store);
    if let Some((_, held)) = &current {
        if held.same_values(record) {
            return RecordWrite::Unchanged;
        }
    }
    let (target, sequence) = match &current {
        Some((slot, held)) => (SLOTS - 1 - slot, held.sequence.wrapping_add(1)),
        None => (0, 1),
    };
    let mut written = record.clone();
    written.sequence = sequence;
    let mut bytes = written.encode();
    let programmed = store.program(target, &bytes);
    bytes.fill(0);
    core::hint::black_box(&bytes);
    let read_back = programmed && Record::decode(store.read(target)).is_some_and(|back| back.same_values(record));
    if !read_back {
        let _ = store.erase(target);
        return RecordWrite::Failed;
    }
    if let Some((slot, _)) = current {
        if !store.erase(slot) && !store.erase(slot) {
            return RecordWrite::Failed;
        }
    }
    RecordWrite::Written
}

/// Changes the boot setting of the stored record.
pub fn set_boot(store: &mut impl RecordStore, boot: BootConnection) -> RecordWrite {
    let Some((_, mut record)) = load(store) else {
        return RecordWrite::NoRecord;
    };
    if record.boot == boot {
        return RecordWrite::Unchanged;
    }
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

    /// Two erase units in memory, counting what was asked of them, with a switch that makes the
    /// next program or erase fail.
    struct Flash {
        units: [[u8; SLOT_SPAN]; SLOTS],
        programs: Vec<usize>,
        erases: Vec<usize>,
        fail_program: bool,
        fail_erase: bool,
    }

    impl Flash {
        fn new() -> Self {
            Flash { units: [[0xFF; SLOT_SPAN]; SLOTS], programs: Vec::new(), erases: Vec::new(), fail_program: false, fail_erase: false }
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
            self.units[slot] = [0xFF; SLOT_SPAN];
            true
        }

        fn program(&mut self, slot: usize, bytes: &[u8]) -> bool {
            self.programs.push(slot);
            assert!(self.units[slot].iter().all(|&b| b == 0xFF), "a program only ever lands on an erased slot");
            if self.fail_program {
                let half = bytes.len() / 2;
                self.units[slot][..half].copy_from_slice(&bytes[..half]);
                return false;
            }
            self.units[slot][..bytes.len()].copy_from_slice(bytes);
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
        assert_eq!(flash.units[0], [0xFF; SLOT_SPAN], "the old slot reads erased");
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
        assert_eq!(flash.units[0], [0xFF; SLOT_SPAN], "the superseded slot was erased by the read");
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
        assert_eq!(flash.units[1], [0xFF; SLOT_SPAN], "the torn slot was erased");
        assert_eq!(load(&mut flash).unwrap().1.ssid(), b"kept");
    }

    /// A torn slot found on a read is erased too: its bytes are not a record worth trying, and they
    /// may hold part of a secret.
    #[test]
    fn a_slot_that_is_neither_erased_nor_a_record_is_erased_on_read() {
        let mut flash = Flash::new();
        flash.units[1][..8].copy_from_slice(b"LWFRgarb");
        assert!(load(&mut flash).is_none());
        assert_eq!(flash.units[1], [0xFF; SLOT_SPAN]);
    }

    #[test]
    fn a_zeroed_slot_and_a_bad_checksum_read_as_no_record() {
        let mut flash = Flash::new();
        store(&mut flash, &network(b"bench-net", b"secret-one"));
        flash.units[0][AT_NAME] ^= 1;
        assert!(Record::decode(&flash.units[0]).is_none(), "one flipped bit fails the check");
        assert!(Record::decode(&[0u8; SLOT_SPAN]).is_none(), "a zeroed slot is empty");
        assert!(Record::decode(&[0xFFu8; SLOT_SPAN]).is_none(), "an erased slot is empty");
    }

    #[test]
    fn clear_erases_both_slots() {
        let mut flash = Flash::new();
        store(&mut flash, &network(b"first", b"secret-one"));
        store(&mut flash, &network(b"second", b"secret-two"));
        assert!(clear(&mut flash));
        assert!(load(&mut flash).is_none());
        assert_eq!(flash.units, [[0xFF; SLOT_SPAN]; SLOTS]);
    }

    #[test]
    fn the_boot_setting_is_rewritten_with_the_rest_kept_and_the_old_slot_erased() {
        let mut flash = Flash::new();
        assert_eq!(set_boot(&mut flash, BootConnection::BeforeMain), RecordWrite::NoRecord);
        store(&mut flash, &network(b"bench-net", b"secret-one"));
        assert_eq!(set_boot(&mut flash, BootConnection::Background), RecordWrite::Unchanged);
        assert_eq!(set_boot(&mut flash, BootConnection::BeforeMain), RecordWrite::Written);
        let (slot, record) = load(&mut flash).unwrap();
        assert_eq!((slot, record.boot, record.secret()), (1, BootConnection::BeforeMain, &b"secret-one"[..]));
        assert_eq!(flash.units[0], [0xFF; SLOT_SPAN]);
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
}
