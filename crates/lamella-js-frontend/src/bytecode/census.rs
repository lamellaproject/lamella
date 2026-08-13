//! Where an artifact's bytes went.

use lamella_js_bytecode::format::Tag;

/// Per-tag counts and byte totals, plus the two uniform overheads worth pricing separately.
#[derive(Debug, Clone, Copy)]
pub struct Stats {
    /// How many nodes of each tag.
    pub counts: [u32; 256],
    /// Total encoded bytes per tag -- tag byte, length field and payload, children included.
    pub bytes: [u32; 256],
    /// Every node in the arena.
    pub nodes: u32,
    /// The node arena, in bytes. Excludes the header, the string pool and the source section.
    pub arena_bytes: u32,
    /// **The format's single largest uniform overhead**, and the first thing to try removing if
    /// flash ever binds: the per-node payload-length varint that makes skipping a subtree O(1).
    /// Named separately because a cost everything pays at once is invisible in a total.
    pub len_field_bytes: u32,
    /// One tag byte per node.
    pub tag_bytes: u32,
    /// The two varints of every span. This is what carrying diagnostics costs before the optional
    /// source section is even considered.
    pub span_bytes: u32,
    /// Distinct strings in the pool, and the bytes their blobs occupy.
    pub strings: u32,
    pub string_bytes: u32,
    /// What the pool would have cost with no deduplication -- so the saving is a number rather
    /// than a claim.
    pub string_bytes_undeduped: u32,
}

impl Default for Stats {
    fn default() -> Self {
        Self {
            counts: [0; 256],
            bytes: [0; 256],
            nodes: 0,
            arena_bytes: 0,
            len_field_bytes: 0,
            tag_bytes: 0,
            span_bytes: 0,
            strings: 0,
            string_bytes: 0,
            string_bytes_undeduped: 0,
        }
    }
}

impl Stats {
    /// Every tag that occurs, with its count and byte total, most bytes first is the caller's job.
    pub fn per_tag(&self) -> impl Iterator<Item = (Tag, u32, u32)> + '_ {
        (0u16..256).filter_map(move |byte| {
            let byte = byte as u8;
            let count = self.counts[byte as usize];
            if count == 0 {
                return None;
            }
            Tag::from_u8(byte).map(|tag| (tag, count, self.bytes[byte as usize]))
        })
    }

    /// Bytes attributed to a category of tag, for the coarse breakdown.
    #[must_use]
    pub fn category_bytes(&self, category: &str) -> u32 {
        let mut total = 0;
        for byte in 0u16..256 {
            let byte = byte as u8;
            if self.counts[byte as usize] == 0 {
                continue;
            }
            if let Some(tag) = Tag::from_u8(byte) {
                if tag.category() == category {
                    total += self.bytes[byte as usize];
                }
            }
        }
        total
    }

    /// Mean bytes per node, times a thousand -- integer arithmetic, so the census stays linkable
    /// on a no-float profile.
    #[must_use]
    pub fn milli_bytes_per_node(&self) -> u32 {
        if self.nodes == 0 {
            return 0;
        }
        (u64::from(self.arena_bytes) * 1000 / u64::from(self.nodes)) as u32
    }
}
