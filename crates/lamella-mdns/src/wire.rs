//! The DNS message format, as multicast DNS uses it.

extern crate alloc;

use alloc::vec::Vec;

/// The mDNS UDP port (RFC 6762, section 2).
pub const PORT: u16 = 5353;

/// The IPv4 link-local multicast address for mDNS (RFC 6762, section 3), in network order.
pub const MULTICAST_V4: [u8; 4] = [224, 0, 0, 251];

/// The IPv6 equivalent (RFC 6762, section 3), in network order.
pub const MULTICAST_V6: [u8; 16] = [0xFF, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xFB];

/// The internet class.
pub const CLASS_IN: u16 = 1;

/// The bit multicast DNS takes over in a record's class field: this record REPLACES what the peer
/// holds rather than joining it (RFC 6762, section 10.2).
pub const CACHE_FLUSH: u16 = 0x8000;

/// The bit multicast DNS takes over in a question's class field: answer me directly rather than to
/// the group (RFC 6762, section 5.4).
pub const UNICAST_RESPONSE: u16 = 0x8000;

/// Record types this implementation reads or writes.
pub mod kind {
    /// An IPv4 address.
    pub const A: u16 = 1;
    /// A pointer, used by service discovery to enumerate instances of a service.
    pub const PTR: u16 = 12;
    /// Structured key/value data about a service instance.
    pub const TXT: u16 = 16;
    /// An IPv6 address.
    pub const AAAA: u16 = 28;
    /// The host and port a service instance is reached at.
    pub const SRV: u16 = 33;
    /// A request for every record of a name (RFC 1035, 3.2.3).
    pub const ANY: u16 = 255;
}

/// A label is at most 63 octets, because the two high bits of a length byte are reserved to mark a
/// compression pointer (RFC 1035, 4.1.4 -- "labels are restricted to 63 octets or less").
pub const MAX_LABEL: usize = 63;

/// A name is at most 255 octets (RFC 1035, 2.3.4).
pub const MAX_NAME: usize = 255;

/// How many compression pointers one name may follow before the reader gives up.
///
/// A pointer may point at another pointer, and nothing in the format stops one pointing at itself.
/// **A reader without this bound is a reader a single malformed datagram can hang**, and on a
/// device that datagram arrives from anybody on the network. The limit is deliberately small: a
/// legitimate name needs one or two.
const MAX_POINTER_HOPS: usize = 16;

/// Why a message could not be read.
///
/// Every variant is a datagram that arrived from the network, so none of them is a defect in the
/// reader -- they are the shapes a stranger can send. A responder answers none of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadError {
    /// The bytes ran out mid-field.
    Truncated,
    /// A label longer than the format allows, or a name longer than the format allows.
    Oversized,
    /// Compression pointers that loop, or nest further than any real name needs.
    PointerLoop,
}

/// A question: what is being asked, and how it wants to be answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Question {
    /// The name asked about, as labels without the trailing root.
    pub labels: Vec<Vec<u8>>,
    /// The record type asked for.
    pub kind: u16,
    /// The class, with [`UNICAST_RESPONSE`] already stripped -- see [`Question::wants_unicast`].
    pub class: u16,
    /// Whether the asker set the unicast-response bit (RFC 6762, 5.4).
    pub wants_unicast: bool,
}

/// One resource record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// The name this record is about.
    pub labels: Vec<Vec<u8>>,
    /// The record type.
    pub kind: u16,
    /// The class, without the cache-flush bit.
    pub class: u16,
    /// Whether the cache-flush bit is set (RFC 6762, 10.2).
    pub cache_flush: bool,
    /// Seconds a peer may keep this.
    pub ttl: u32,
    /// The record's payload, already in wire form.
    pub data: Vec<u8>,
}

/// A parsed message. Only the parts a responder needs: the header flags, the questions, and the
/// records a peer already claims to know.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Message {
    /// The transaction id. Multicast DNS senders normally use zero (RFC 6762, section 18.1), and a
    /// response echoes whatever the query used.
    pub id: u16,
    /// Whether this is a response rather than a query.
    pub response: bool,
    /// The questions asked.
    pub questions: Vec<Question>,
    /// Records carried as answers -- in a query these are known answers the asker already holds.
    pub answers: Vec<Record>,
}

/// Read a name at `at`, following compression pointers, and return it with the offset just past the
/// name AS IT APPEARED (not past whatever a pointer led to).
///
/// The distinction is the one that makes a parser correct: after a name that ended in a pointer,
/// the next field follows the pointer's two bytes, not the bytes at the far end of it.
fn read_name(buf: &[u8], at: usize) -> Result<(Vec<Vec<u8>>, usize), ReadError> {
    let mut labels = Vec::new();
    let mut cursor = at;
    let mut after: Option<usize> = None;
    let mut hops = 0usize;
    let mut total = 0usize;

    loop {
        let length = *buf.get(cursor).ok_or(ReadError::Truncated)?;
        match length & 0xC0 {
            0 => {
                if length == 0 {
                    cursor += 1;
                    break;
                }
                let length = usize::from(length);
                if length > MAX_LABEL {
                    return Err(ReadError::Oversized);
                }
                let start = cursor + 1;
                let end = start + length;
                let label = buf.get(start..end).ok_or(ReadError::Truncated)?;
                total += length + 1;
                if total > MAX_NAME {
                    return Err(ReadError::Oversized);
                }
                labels.push(label.to_vec());
                cursor = end;
            }
            0xC0 => {
                let low = *buf.get(cursor + 1).ok_or(ReadError::Truncated)?;
                let target = usize::from(u16::from_be_bytes([length & 0x3F, low]));
                hops += 1;
                if hops > MAX_POINTER_HOPS {
                    return Err(ReadError::PointerLoop);
                }
                if after.is_none() {
                    after = Some(cursor + 2);
                }
                if target >= buf.len() {
                    return Err(ReadError::Truncated);
                }
                cursor = target;
            }
            _ => return Err(ReadError::Truncated),
        }
    }
    Ok((labels, after.unwrap_or(cursor)))
}

fn read_u16(buf: &[u8], at: usize) -> Result<u16, ReadError> {
    let bytes = buf.get(at..at + 2).ok_or(ReadError::Truncated)?;
    Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
}

fn read_u32(buf: &[u8], at: usize) -> Result<u32, ReadError> {
    let bytes = buf.get(at..at + 4).ok_or(ReadError::Truncated)?;
    Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

impl Message {
    /// Parse a datagram.
    ///
    /// # Errors
    /// A [`ReadError`] for anything malformed. Every one of these arrives from the network, so a
    /// caller should treat them as ordinary traffic to ignore rather than as a fault.
    pub fn read(buf: &[u8]) -> Result<Self, ReadError> {
        let id = read_u16(buf, 0)?;
        let flags = read_u16(buf, 2)?;
        let questions_count = read_u16(buf, 4)?;
        let answers_count = read_u16(buf, 6)?;
        let mut at = 12;

        let mut questions = Vec::new();
        for _ in 0..questions_count {
            let (labels, next) = read_name(buf, at)?;
            let kind = read_u16(buf, next)?;
            let class = read_u16(buf, next + 2)?;
            at = next + 4;
            questions.push(Question {
                labels,
                kind,
                class: class & !UNICAST_RESPONSE,
                wants_unicast: class & UNICAST_RESPONSE != 0,
            });
        }

        let mut answers = Vec::new();
        for _ in 0..answers_count {
            let (labels, next) = read_name(buf, at)?;
            let kind = read_u16(buf, next)?;
            let class = read_u16(buf, next + 2)?;
            let ttl = read_u32(buf, next + 4)?;
            let length = usize::from(read_u16(buf, next + 8)?);
            let start = next + 10;
            let data = buf.get(start..start + length).ok_or(ReadError::Truncated)?;
            at = start + length;
            answers.push(Record {
                labels,
                kind,
                class: class & !CACHE_FLUSH,
                cache_flush: class & CACHE_FLUSH != 0,
                ttl,
                data: data.to_vec(),
            });
        }

        Ok(Self { id, response: flags & 0x8000 != 0, questions, answers })
    }
}

/// Append a name in wire form, uncompressed.
///
/// Deliberately never compressed on the way out. Compression saves bytes a responder on a link
/// like this does not need to save, and a compression bug produces a message that parses into
/// something other than what was meant -- which is far more expensive than the bytes. Reading
/// compression is not optional, because other implementations use it; writing it is.
pub fn write_name(out: &mut Vec<u8>, labels: &[Vec<u8>]) {
    for label in labels {
        let length = label.len().min(MAX_LABEL);
        out.push(length as u8);
        out.extend_from_slice(&label[..length]);
    }
    out.push(0);
}

/// Build a response datagram carrying `answers`.
///
/// The id is echoed from the query it answers. Multicast DNS senders normally use zero, but echoing
/// is what makes a unicast reply match up for an asker that did not (RFC 6762, section 18.1).
#[must_use]
pub fn write_response(id: u16, answers: &[Record]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(&0x8400u16.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(&(answers.len() as u16).to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes());
    for record in answers {
        write_name(&mut out, &record.labels);
        out.extend_from_slice(&record.kind.to_be_bytes());
        let class = record.class | if record.cache_flush { CACHE_FLUSH } else { 0 };
        out.extend_from_slice(&class.to_be_bytes());
        out.extend_from_slice(&record.ttl.to_be_bytes());
        out.extend_from_slice(&(record.data.len() as u16).to_be_bytes());
        out.extend_from_slice(&record.data);
    }
    out
}
