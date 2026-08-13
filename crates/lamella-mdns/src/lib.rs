//! Being findable on a network without anybody knowing the address.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod wire;

use alloc::vec::Vec;
use wire::{CLASS_IN, Message, Question, Record, kind};

/// The DNS-SD service type this protocol is discovered under.
///
/// A service type is `_<name>._<protocol>` (RFC 6763, section 4.1.2), and the name is limited to
/// fifteen characters. Registered with no authority; the pairing with the port is what identifies
/// the service, and both live in one place so they cannot drift apart.
pub const SERVICE: [&[u8]; 2] = [b"_lamella-link", b"_tcp"];

/// The domain service discovery uses on a local link (RFC 6763, section 4.1.3).
pub const LOCAL: &[u8] = b"local";

/// Seconds a peer may cache a record of ours.
///
/// RFC 6763 section 10 recommends 75 minutes for records that are not host names, and 120 seconds
/// for the address records -- shorter, because an address is the part most likely to become wrong
/// while the service itself has not changed.
const TTL_SERVICE: u32 = 4500;
const TTL_ADDRESS: u32 = 120;

/// What a board says about itself.
///
/// The name is the part a person reads. RFC 6763 section 4.1.1 is explicit that an instance name is
/// meant to be user-visible and user-friendly -- so it is free text a person chooses, not a
/// mangled serial number, and two boards on one desk should not both be called the same thing.
#[derive(Clone, Debug)]
pub struct Advertisement {
    /// The instance name a person sees, as they typed it.
    pub name: Vec<u8>,
    /// The host name to publish addresses under, without the domain.
    pub host: Vec<u8>,
    /// The port the protocol is served on.
    pub port: u16,
    /// The board's IPv4 address in network order, if it has one.
    pub ipv4: Option<[u8; 4]>,
    /// Key/value pairs published in the TXT record, each already `key=value` (RFC 6763, section 6).
    pub txt: Vec<Vec<u8>>,
}

impl Advertisement {
    /// `<Instance>.<Service>.<Domain>` (RFC 6763, section 4.1).
    fn instance_labels(&self) -> Vec<Vec<u8>> {
        let mut labels = Vec::with_capacity(4);
        labels.push(self.name.clone());
        labels.push(SERVICE[0].to_vec());
        labels.push(SERVICE[1].to_vec());
        labels.push(LOCAL.to_vec());
        labels
    }

    /// `<Service>.<Domain>` -- what a host browsing for this kind of service asks about.
    fn service_labels(&self) -> Vec<Vec<u8>> {
        alloc::vec![SERVICE[0].to_vec(), SERVICE[1].to_vec(), LOCAL.to_vec()]
    }

    /// `<host>.local`.
    fn host_labels(&self) -> Vec<Vec<u8>> {
        alloc::vec![self.host.clone(), LOCAL.to_vec()]
    }

    /// The PTR record: this service type has this instance (RFC 6763, section 4.1).
    ///
    /// NOT cache-flush. A PTR is SHARED -- several boards answer the same browse and each adds
    /// an instance. Setting the bit would tell a peer that this responder's answer replaces the
    /// others, so a second board appearing would make the first vanish from the list.
    fn ptr(&self) -> Record {
        let mut data = Vec::new();
        wire::write_name(&mut data, &self.instance_labels());
        Record {
            labels: self.service_labels(),
            kind: kind::PTR,
            class: CLASS_IN,
            cache_flush: false,
            ttl: TTL_SERVICE,
            data,
        }
    }

    /// The SRV record: which host and port this instance is at (RFC 6763, section 4.1).
    ///
    /// Cache-flush: an instance has ONE location, so this answer replaces whatever a peer held.
    fn srv(&self) -> Record {
        let mut data = Vec::new();
        data.extend_from_slice(&0u16.to_be_bytes());
        data.extend_from_slice(&0u16.to_be_bytes());
        data.extend_from_slice(&self.port.to_be_bytes());
        wire::write_name(&mut data, &self.host_labels());
        Record {
            labels: self.instance_labels(),
            kind: kind::SRV,
            class: CLASS_IN,
            cache_flush: true,
            ttl: TTL_SERVICE,
            data,
        }
    }

    /// The TXT record, each entry a length byte then `key=value` (RFC 6763, section 6.1).
    ///
    /// An EMPTY TXT is a single zero byte, not an empty record. Section 6.1 requires that a
    /// service always has a TXT record and that an empty one carries one zero-length string --
    /// because a missing TXT and an empty TXT mean different things to a browser, and a
    /// zero-length record is the one that reads as "still resolving".
    fn txt(&self) -> Record {
        let mut data = Vec::new();
        for entry in &self.txt {
            let length = entry.len().min(255);
            data.push(length as u8);
            data.extend_from_slice(&entry[..length]);
        }
        if data.is_empty() {
            data.push(0);
        }
        Record {
            labels: self.instance_labels(),
            kind: kind::TXT,
            class: CLASS_IN,
            cache_flush: true,
            ttl: TTL_SERVICE,
            data,
        }
    }

    /// The A record, when the board has an address to publish.
    fn a(&self) -> Option<Record> {
        let ipv4 = self.ipv4?;
        Some(Record {
            labels: self.host_labels(),
            kind: kind::A,
            class: CLASS_IN,
            cache_flush: true,
            ttl: TTL_ADDRESS,
            data: ipv4.to_vec(),
        })
    }

    /// Everything this board would announce, in the order a browser wants it: the pointer that
    /// names the instance, then the records that resolve it.
    ///
    /// A response carries the ADDITIONAL records rather than making the asker come back for them
    /// (RFC 6763, section 12): a browse answered with only a PTR costs two more round trips, and on
    /// a link where every one of them is a multicast that is a cost everybody on the network pays.
    #[must_use]
    pub fn records(&self) -> Vec<Record> {
        let mut records = alloc::vec![self.ptr(), self.srv(), self.txt()];
        records.extend(self.a());
        records
    }
}

/// Answers questions about one board, and says nothing otherwise.
#[derive(Clone, Debug)]
pub struct Responder {
    advertisement: Advertisement,
}

/// What a responder decided to do about a datagram.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reply {
    /// The datagram to send.
    pub datagram: Vec<u8>,
    /// Whether the asker wanted it directly rather than to the group (RFC 6762, section 5.4).
    pub unicast: bool,
}

impl Responder {
    /// A responder for one board.
    #[must_use]
    pub fn new(advertisement: Advertisement) -> Self {
        Self { advertisement }
    }

    /// The advertisement, so a caller can announce unprompted as well as answer.
    #[must_use]
    pub fn advertisement(&self) -> &Advertisement {
        &self.advertisement
    }

    /// An unsolicited announcement of everything this board offers (RFC 6762, section 8.3).
    #[must_use]
    pub fn announcement(&self) -> Vec<u8> {
        wire::write_response(0, &self.advertisement.records())
    }

    /// Consider one received datagram, and answer if it asked about us.
    ///
    /// `None` for everything else, which is nearly all of it: an mDNS group is busy with other
    /// people's traffic, and a responder that replied to anything it merely parsed would be a
    /// device shouting over its neighbors.
    #[must_use]
    pub fn handle(&self, datagram: &[u8]) -> Option<Reply> {
        let message = Message::read(datagram).ok()?;
        if message.response {
            return None;
        }

        let mut answers: Vec<Record> = Vec::new();
        let mut unicast = false;
        for question in &message.questions {
            let matched = self.answer(question);
            if !matched.is_empty() && question.wants_unicast {
                unicast = true;
            }
            for record in matched {
                if !answers.iter().any(|held| held.labels == record.labels && held.kind == record.kind) {
                    answers.push(record);
                }
            }
        }
        if answers.is_empty() {
            return None;
        }

        answers.retain(|record| {
            !message.answers.iter().any(|known| {
                known.labels == record.labels && known.kind == record.kind && known.data == record.data
            })
        });
        if answers.is_empty() {
            return None;
        }

        Some(Reply { datagram: wire::write_response(message.id, &answers), unicast })
    }

    /// The records that answer one question, or nothing.
    fn answer(&self, question: &Question) -> Vec<Record> {
        if question.class != CLASS_IN {
            return Vec::new();
        }
        let advertisement = &self.advertisement;
        let wants = |kind_wanted: u16| question.kind == kind_wanted || question.kind == kind::ANY;

        if question.labels == advertisement.service_labels() && wants(kind::PTR) {
            return advertisement.records();
        }
        if question.labels == advertisement.instance_labels() {
            let mut records = Vec::new();
            if wants(kind::SRV) {
                records.push(advertisement.srv());
            }
            if wants(kind::TXT) {
                records.push(advertisement.txt());
            }
            if !records.is_empty() {
                records.extend(advertisement.a());
            }
            return records;
        }
        if question.labels == advertisement.host_labels() && wants(kind::A) {
            return advertisement.a().into_iter().collect();
        }
        Vec::new()
    }
}
