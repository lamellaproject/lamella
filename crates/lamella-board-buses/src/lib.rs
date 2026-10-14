//! The bus table of the board a build names: how it is laid out, the check made before any of it is
//! served, and the two lookups `System.Device`'s factories make through `Lamella.Hardware.BoardBuses`.
//!
//! A host writes the table from the board's `board.toml` as little-endian `u32` words. The header is
//! "LBUS" ([`MAGIC`]), the layout version ([`VERSION`]), the board's product model and the row count.
//! Each row follows as its bus kind, its bus number, its word count `n` and its `n` words, in
//! kind-then-bus order. A row's words are the binding its family's driver is constructed from.
//!
//! The kinds are [`GPIO`] (bus 0 is the board's controller), [`I2C`] and [`SPI`]. Kinds 4 to 6 (PWM,
//! ADC and UART) are reserved, and take the same row shape.
//!
//! Every reader takes the table's words through a closure, so the table can sit in a slice, in an
//! image's little-endian bytes, or behind a linked symbol, and nothing here allocates.

#![no_std]

/// The first word of every bus table: "LBUS" in little-endian bytes.
pub const MAGIC: u32 = 0x5355_424C;

/// The one layout version this crate reads.
pub const VERSION: u32 = 1;

/// How many words the header holds: [`MAGIC`], the version, the board's product model and the row
/// count.
pub const HEADER_WORDS: usize = 4;

/// The GPIO kind. Bus 0 is the board's controller.
pub const GPIO: u32 = 1;

/// The I2C kind.
pub const I2C: u32 = 2;

/// The SPI kind.
pub const SPI: u32 = 3;

/// What is wrong with a bus table that is refused rather than served.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Fault {
    /// It is shorter than its header, or its first word is not "LBUS".
    NotABusTable,
    /// Its layout version is not the one this crate reads.
    UnsupportedVersion(u32),
    /// A row runs past the table's last word.
    RowPastEnd {
        /// The row's position in the table, counted from 0.
        row: u32,
    },
    /// A row has no descriptor words, so its bus would read as one the board does not have.
    EmptyRow {
        /// The row's bus kind.
        kind: u32,
        /// The row's bus number.
        bus: u32,
    },
    /// A row does not follow the one before it in kind-then-bus order, a repeated row included, so a
    /// lookup could stop before it reached that row.
    OutOfOrder {
        /// The row's bus kind.
        kind: u32,
        /// The row's bus number.
        bus: u32,
    },
    /// Words follow the last row the header counts.
    WordsAfterLastRow,
}

impl core::fmt::Display for Fault {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Fault::NotABusTable => f.write_str("it does not begin with the four-word header and \"LBUS\""),
            Fault::UnsupportedVersion(version) => {
                write!(f, "its layout version is {version}, and this build reads version {VERSION}")
            }
            Fault::RowPastEnd { row } => write!(f, "row {row} runs past the table's last word"),
            Fault::EmptyRow { kind, bus } => {
                write!(f, "the row for kind {kind}, bus {bus} has no descriptor words")
            }
            Fault::OutOfOrder { kind, bus } => write!(
                f,
                "the row for kind {kind}, bus {bus} does not follow the row before it in kind-then-bus \
                 order"
            ),
            Fault::WordsAfterLastRow => f.write_str("words follow the last row its header counts"),
        }
    }
}

/// Checks the bus table `table` before any of it is served. See [`check_words`].
///
/// # Errors
/// The [`Fault`] that keeps the table from being served.
pub fn check(table: &[u32]) -> Result<(), Fault> {
    check_words(table.len(), |index| table.get(index).copied())
}

/// Checks a bus table of `len` words, read through `read`, before any of it is served: the header,
/// then each row the header counts, and no word after the last. Each row must lie inside the table,
/// hold at least one word, and follow the row before it in kind-then-bus order.
///
/// A header that counts no rows is a table: the board it describes has no buses.
///
/// # Errors
/// The [`Fault`] that keeps the table from being served.
pub fn check_words(len: usize, read: impl Fn(usize) -> Option<u32>) -> Result<(), Fault> {
    let word = |index: usize| if index < len { read(index) } else { None };
    if word(0) != Some(MAGIC) {
        return Err(Fault::NotABusTable);
    }
    let (Some(version), Some(rows)) = (word(1), word(3)) else {
        return Err(Fault::NotABusTable);
    };
    if version != VERSION {
        return Err(Fault::UnsupportedVersion(version));
    }
    let mut at = HEADER_WORDS;
    let mut previous: Option<(u32, u32)> = None;
    for row in 0..rows {
        let past_end = Fault::RowPastEnd { row };
        let (Some(kind), Some(bus), Some(count)) = (word(at), word(at + 1), word(at + 2)) else {
            return Err(past_end);
        };
        if count == 0 {
            return Err(Fault::EmptyRow { kind, bus });
        }
        if previous.is_some_and(|before| before >= (kind, bus)) {
            return Err(Fault::OutOfOrder { kind, bus });
        }
        at = descriptor_end(at, count).filter(|&end| end <= len).ok_or(past_end)?;
        previous = Some((kind, bus));
    }
    if at != len {
        return Err(Fault::WordsAfterLastRow);
    }
    Ok(())
}

/// How many descriptor words bus `bus` of `kind` has in the table read through `read`: -1 when there
/// is no table (its first word is not "LBUS"), 0 when the table has no such bus, a negative argument
/// included, and the row's word count otherwise. What `Lamella.Hardware.BoardBuses.Length` answers.
///
/// The table is one [`check_words`] accepted.
pub fn length(read: impl Fn(usize) -> Option<u32>, kind: i32, bus: i32) -> i32 {
    if read(0) != Some(MAGIC) {
        return -1;
    }
    row(&read, kind, bus).map_or(0, |(_, count)| i32::try_from(count).unwrap_or(0))
}

/// Word `index` of the descriptor of bus `bus` of `kind` in the table read through `read`, and 0 for
/// an index outside it, a bus the table does not have, or no table. What
/// `Lamella.Hardware.BoardBuses.Word` answers.
///
/// The table is one [`check_words`] accepted.
pub fn word(read: impl Fn(usize) -> Option<u32>, kind: i32, bus: i32, index: i32) -> u32 {
    if read(0) != Some(MAGIC) {
        return 0;
    }
    let Ok(index) = u32::try_from(index) else {
        return 0;
    };
    row(&read, kind, bus)
        .filter(|&(_, count)| index < count)
        .and_then(|(first, _)| read(first.checked_add(index as usize)?))
        .unwrap_or(0)
}

/// Where the descriptor of bus `bus` of `kind` begins, and how many words it holds; `None` when the
/// table has no such row.
fn row(read: &impl Fn(usize) -> Option<u32>, kind: i32, bus: i32) -> Option<(usize, u32)> {
    let (Ok(kind), Ok(bus)) = (u32::try_from(kind), u32::try_from(bus)) else {
        return None;
    };
    let rows = read(3)?;
    let mut at = HEADER_WORDS;
    for _ in 0..rows {
        let (row_kind, row_bus, count) = (read(at)?, read(at + 1)?, read(at + 2)?);
        match (row_kind, row_bus).cmp(&(kind, bus)) {
            core::cmp::Ordering::Equal => return Some((at + 3, count)),
            core::cmp::Ordering::Greater => return None,
            core::cmp::Ordering::Less => at = descriptor_end(at, count)?,
        }
    }
    None
}

/// The word after the row that begins at `at` and holds `count` descriptor words.
fn descriptor_end(at: usize, count: u32) -> Option<usize> {
    at.checked_add(3)?.checked_add(usize::try_from(count).ok()?)
}

#[cfg(test)]
mod tests {
    extern crate alloc;
    use super::*;
    use alloc::vec::Vec;

    /// A table with two rows: I2C bus 0 of three words and SPI bus 1 of two.
    fn two_buses() -> Vec<u32> {
        alloc::vec![
            MAGIC, 1, 2, 2,
            I2C, 0, 3, 0x4009_0000, 0x250, 150_000_000,
            SPI, 1, 2, 0x4008_0000, 17,
        ]
    }

    fn slice(table: &[u32]) -> impl Fn(usize) -> Option<u32> + '_ {
        |index| table.get(index).copied()
    }

    /// Every table a reader would misread is refused, by name.
    #[test]
    fn a_table_is_checked_before_it_is_served() {
        let good = two_buses();
        assert_eq!(check(&good), Ok(()));
        let with = |edit: &dyn Fn(&mut Vec<u32>)| {
            let mut table = good.clone();
            edit(&mut table);
            table
        };
        let cases: [(&str, Vec<u32>, Fault); 10] = [
            ("empty", Vec::new(), Fault::NotABusTable),
            ("a short header", alloc::vec![MAGIC, 1, 2], Fault::NotABusTable),
            ("another magic", with(&|table| table[0] = 0x4C42_5553), Fault::NotABusTable),
            ("version 2", with(&|table| table[1] = 2), Fault::UnsupportedVersion(2)),
            ("one row counted too many", with(&|table| table[3] = 3), Fault::RowPastEnd { row: 2 }),
            ("a count past the end", with(&|table| table[12] = 3), Fault::RowPastEnd { row: 1 }),
            (
                "a row of no words",
                with(&|table| {
                    table[6] = 0;
                    table.truncate(7);
                    table[3] = 1;
                }),
                Fault::EmptyRow { kind: I2C, bus: 0 },
            ),
            (
                "rows out of order",
                with(&|table| {
                    table[4] = SPI;
                    table[5] = 2;
                }),
                Fault::OutOfOrder { kind: SPI, bus: 1 },
            ),
            (
                "a repeated row",
                with(&|table| table[10..12].copy_from_slice(&[I2C, 0])),
                Fault::OutOfOrder { kind: I2C, bus: 0 },
            ),
            ("a word after the last row", with(&|table| table.push(0)), Fault::WordsAfterLastRow),
        ];
        for (what, table, fault) in cases {
            assert_eq!(check(&table), Err(fault), "{what}: {table:x?}");
        }
    }

    /// A table's lookups answer its rows word for word, 0 for a bus it lacks or a word outside a
    /// row, and never the next row's first word.
    #[test]
    fn the_lookups_answer_the_table_word_for_word() {
        let table = two_buses();
        let read = slice(&table);
        assert_eq!(length(&read, 2, 0), 3);
        assert_eq!(length(&read, 3, 1), 2);
        let i2c: Vec<u32> = (0..3).map(|index| word(&read, 2, 0, index)).collect();
        assert_eq!(i2c, [0x4009_0000, 0x250, 150_000_000]);
        let spi: Vec<u32> = (0..2).map(|index| word(&read, 3, 1, index)).collect();
        assert_eq!(spi, [0x4008_0000, 17]);
        for (kind, bus) in [(1, 0), (2, 1), (3, 0), (4, 0), (-1, 0), (2, -1)] {
            assert_eq!(length(&read, kind, bus), 0, "kind {kind}, bus {bus}");
            assert_eq!(word(&read, kind, bus, 0), 0, "kind {kind}, bus {bus}");
        }
        assert_eq!(word(&read, 2, 0, 3), 0, "past I2C bus 0's last word");
        assert_eq!(word(&read, 2, 0, -1), 0, "a negative index");
    }

    /// No table answers -1 for every bus, whether nothing is there or a word that is not "LBUS" is:
    /// a linked image's one-word default when no board is named. A table of no rows answers 0.
    #[test]
    fn no_table_answers_minus_one_and_a_table_of_no_rows_answers_zero() {
        for (what, table) in [("nothing", &[][..]), ("a one-word default", &[0][..])] {
            let read = slice(table);
            for (kind, bus) in [(1, 0), (2, 0), (3, 0), (-1, -1)] {
                assert_eq!(length(&read, kind, bus), -1, "{what}: kind {kind}, bus {bus}");
                assert_eq!(word(&read, kind, bus, 0), 0, "{what}: kind {kind}, bus {bus}");
            }
        }
        let empty = [MAGIC, 1, 2, 0];
        assert_eq!(check(&empty), Ok(()));
        let read = slice(&empty);
        for (kind, bus) in [(1, 0), (2, 0), (3, 0), (-1, -1)] {
            assert_eq!(length(&read, kind, bus), 0, "no rows: kind {kind}, bus {bus}");
        }
    }

    /// A table held as little-endian bytes at no particular alignment, as an image holds it, reads
    /// as the same words.
    #[test]
    fn a_table_read_out_of_unaligned_little_endian_bytes_answers_the_same() {
        let table = two_buses();
        let mut bytes: Vec<u8> = alloc::vec![0xAA];
        for word in &table {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
        let read = |index: usize| {
            let at = 1 + index * 4;
            bytes.get(at..at + 4).map(|word| u32::from_le_bytes([word[0], word[1], word[2], word[3]]))
        };
        assert_eq!(check_words(table.len(), &read), Ok(()));
        for index in 0..3 {
            assert_eq!(word(&read, 2, 0, index), word(slice(&table), 2, 0, index));
        }
        assert_eq!(length(&read, 3, 1), 2);
    }
}
