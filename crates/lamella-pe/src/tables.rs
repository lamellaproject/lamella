//! The metadata tables stream `#~` (ECMA-335 1st ed, II.24.2.6).

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use lamella_metadata::CodedIndex;
use lamella_token::Token;

/// Which variable-width heaps use 4-byte offsets (the HeapSizes byte, II.24.2.6).
#[derive(Clone, Copy, Default, Debug)]
pub struct HeapSizes {
    /// `#Strings` offsets are 4 bytes.
    pub wide_strings: bool,
    /// `#GUID` indices are 4 bytes.
    pub wide_guid: bool,
    /// `#Blob` offsets are 4 bytes.
    pub wide_blob: bool,
}

impl HeapSizes {
    const STRINGS: u8 = 0x01;
    const GUID: u8 = 0x02;
    const BLOB: u8 = 0x04;

    /// The heap size, in bytes, from which references into that heap are 4 bytes wide.
    pub const WIDE_FROM: usize = 0x1_0000;

    /// The widths the heaps themselves call for, given each heap's bytes as the image holds them
    /// (before the stream's padding): a heap of [`WIDE_FROM`](Self::WIDE_FROM) bytes or more is
    /// referenced with 4-byte values, a smaller one with 2-byte values.
    ///
    /// A `#Strings` or `#Blob` reference is a byte offset, so it exceeds 0xFFFF only in a heap of
    /// 0x10000 bytes or more. A `#GUID` reference is a 1-based index, which would stay under
    /// 0x10000 far longer, but the byte threshold is the one .NET's own metadata writer applies to
    /// all three heaps, so the flags agree with an assembly csc builds from the same heaps.
    #[must_use]
    pub fn for_heaps(strings: &[u8], guids: &[u8], blobs: &[u8]) -> HeapSizes {
        HeapSizes {
            wide_strings: strings.len() >= Self::WIDE_FROM,
            wide_guid: guids.len() >= Self::WIDE_FROM,
            wide_blob: blobs.len() >= Self::WIDE_FROM,
        }
    }

    /// The HeapSizes byte.
    #[must_use]
    pub fn flags(self) -> u8 {
        let mut flags = 0;
        if self.wide_strings {
            flags |= Self::STRINGS;
        }
        if self.wide_guid {
            flags |= Self::GUID;
        }
        if self.wide_blob {
            flags |= Self::BLOB;
        }
        flags
    }
}

/// One cell of a metadata row.
#[derive(Clone, Debug)]
pub enum Column {
    /// A fixed 2-byte value, such as a flags field.
    U16(u16),
    /// A fixed 4-byte value, such as a method-body RVA.
    U32(u32),
    /// A `#Strings` heap offset.
    StringRef(u32),
    /// A `#GUID` heap index.
    GuidRef(u32),
    /// A `#Blob` heap offset.
    BlobRef(u32),
    /// A 1-based row index into a single table.
    Index(u8, u32),
    /// A coded index that may point into one of several tables.
    Coded(CodedIndex, Token),
}

/// The metadata tables being built, each a list of rows in insertion order.
#[derive(Default, Debug)]
pub struct TableStream {
    rows: BTreeMap<u8, Vec<Vec<Column>>>,
    sorted: u64,
    /// Row counts of tables that live in another module but that this stream's columns index --
    /// a standalone Portable PDB's `LocalScope.Method` indexes the image's `MethodDef`. They size
    /// those columns and are never written as rows here.
    external_rows: BTreeMap<u8, u32>,
}

impl TableStream {
    /// An empty set of tables.
    #[must_use]
    pub fn new() -> TableStream {
        TableStream::default()
    }

    /// Appends a row to `table`, returning its 1-based row index.
    pub fn add_row(&mut self, table: u8, columns: Vec<Column>) -> u32 {
        let rows = self.rows.entry(table).or_default();
        rows.push(columns);
        rows.len() as u32
    }

    /// Replaces one cell of an already-added row (1-based `row`), for a value known only after the
    /// row was added -- the assembly version, backfilled from `[assembly: AssemblyVersion]`. A
    /// no-op if the table, row, or column is absent.
    pub fn set_cell(&mut self, table: u8, row: u32, column: usize, value: Column) {
        if let Some(cell) = self
            .rows
            .get_mut(&table)
            .and_then(|rows| rows.get_mut((row as usize).wrapping_sub(1)))
            .and_then(|cells| cells.get_mut(column))
        {
            *cell = value;
        }
    }

    /// One cell of an already-added row (1-based `row`), or `None` when the table, row, or
    /// column is absent. The read half of [`set_cell`](Self::set_cell): it lets a builder
    /// follow a row-index column it wrote earlier -- e.g. walking a `MethodDef`'s `ParamList`
    /// run (II.22.26) -- instead of recomputing the index from the order calls happened to
    /// be made in.
    #[must_use]
    pub fn cell(&self, table: u8, row: u32, column: usize) -> Option<&Column> {
        self.rows
            .get(&table)?
            .get((row as usize).wrapping_sub(1))?
            .get(column)
    }

    /// Records that `table` is emitted in sorted key order, so its bit is set in the
    /// `#~` sorted mask. Some readers reject a sorted-by-spec table (e.g. the PDB's
    /// `LocalScope`) that does not claim it. The caller must actually add the rows
    /// in order.
    pub fn mark_sorted(&mut self, table: u8) {
        self.sorted |= 1u64 << table;
    }

    /// Sorts `table`'s rows by the encoded value of their first column -- a coded parent
    /// index -- and marks it sorted. The required-sorted tables a reader binary-searches by
    /// parent (e.g. `CustomAttribute` by `HasCustomAttribute`) need this when rows are added
    /// out of parent order. Safe only for a table no other table indexes into by row.
    pub fn sort_by_coded_parent(&mut self, table: u8) {
        self.sort_by_coded_column(table, 0);
    }

    /// Sorts `table`'s rows by the encoded value of the coded index in `column` and marks
    /// the table sorted -- for a required-sorted table whose key is not its first column
    /// (`MethodSemantics` sorts by `Association`, its third). The sort is stable, so
    /// same-key rows (a property's getter and setter) keep their emission order. Safe only
    /// for a table no other table indexes into by row.
    pub fn sort_by_coded_column(&mut self, table: u8, column: usize) {
        if let Some(rows) = self.rows.get_mut(&table) {
            rows.sort_by_key(|row| match row.get(column) {
                Some(Column::Coded(kind, token)) => kind.encode(*token),
                _ => 0,
            });
        }
        self.sorted |= 1u64 << table;
    }

    /// Sorts `table` by the coded index in `column`, then rewrites every `Index(table, _)` column
    /// in `dependents` so a row reference that pointed at a row still points at that same row.
    ///
    /// **THIS EXISTS BECAUSE THE PLAIN SORT IS UNSAFE THE MOMENT ANOTHER TABLE INDEXES THIS ONE BY
    /// ROW.** [`TableStream::sort_by_coded_column`] says so in its own doc, and `GenericParam` is
    /// exactly that case: `GenericParamConstraint.Owner` (II.22.21) is a ROW index into
    /// `GenericParam`, so reordering the parameters underneath it silently repoints every
    /// constraint at a different type parameter. Nothing about the resulting assembly is malformed
    /// -- it declares real constraints on the wrong parameters, which is a WRONG ANSWER rather than
    /// a corrupt file, and no structural check would see it.
    ///
    /// The permutation is computed before the move so the old-to-new map is exact; a rewrite that
    /// recomputed positions afterwards would have to assume the sort was stable in a particular way.
    pub fn sort_by_coded_column_remapping(&mut self, table: u8, column: usize, dependents: &[u8]) {
        let Some(rows) = self.rows.get(&table) else {
            self.sorted |= 1u64 << table;
            return;
        };
        let mut order: Vec<(u64, usize)> = rows
            .iter()
            .enumerate()
            .map(|(index, row)| {
                let key = match row.get(column) {
                    Some(Column::Coded(kind, token)) => kind.encode(*token),
                    _ => 0,
                };
                (u64::from(key), index)
            })
            .collect();
        order.sort_by_key(|&(key, index)| (key, index));

        let mut moved_to = alloc::vec![0u32; order.len() + 1];
        for (new_index, &(_, old_index)) in order.iter().enumerate() {
            moved_to[old_index + 1] = (new_index + 1) as u32;
        }

        let rows = self.rows.get_mut(&table).expect("checked above");
        let mut sorted_rows: Vec<Vec<Column>> = Vec::with_capacity(rows.len());
        for &(_, old_index) in &order {
            sorted_rows.push(core::mem::take(&mut rows[old_index]));
        }
        *rows = sorted_rows;
        self.sorted |= 1u64 << table;

        for &dependent in dependents {
            let Some(dependent_rows) = self.rows.get_mut(&dependent) else {
                continue;
            };
            for row in dependent_rows.iter_mut() {
                for cell in row.iter_mut() {
                    if let Column::Index(indexed_table, row_index) = cell
                        && *indexed_table == table
                    {
                        let old = *row_index as usize;
                        if let Some(&new) = moved_to.get(old)
                            && new != 0
                        {
                            *row_index = new;
                        }
                    }
                }
            }
        }
    }

    /// Sorts `table` by the 1-based row index in `column` (an `Index` column), marking it sorted.
    /// For `GenericParamConstraint`, whose key is its `Owner` row index rather than a coded one.
    pub fn sort_by_index_column(&mut self, table: u8, column: usize) {
        if let Some(rows) = self.rows.get_mut(&table) {
            rows.sort_by_key(|row| match row.get(column) {
                Some(Column::Index(_, row_index)) => *row_index,
                _ => 0,
            });
        }
        self.sorted |= 1u64 << table;
    }

    /// The number of rows in `table`.
    #[must_use]
    pub fn row_count(&self, table: u8) -> u32 {
        self.rows.get(&table).map_or(0, |rows| rows.len() as u32)
    }

    /// Records that `table` lives in another module, which holds `rows` rows of it, and that
    /// columns of this stream index it. A standalone Portable PDB is the case: its `LocalScope`
    /// rows name methods of the image's `MethodDef` table, which the PDB does not contain. A reader
    /// sizes an index into such a table from the row count the PDB's `#Pdb` stream declares for
    /// it, so the writer must size it from the same count -- the PDB's own `MethodDef` is empty.
    pub fn set_external_row_count(&mut self, table: u8, rows: u32) {
        self.external_rows.insert(table, rows);
    }

    /// The tables recorded by [`set_external_row_count`](Self::set_external_row_count), as a
    /// Portable PDB's `#Pdb` stream declares them: the bit vector of tables, and their row counts
    /// in ascending table order.
    #[must_use]
    pub fn external_tables(&self) -> (u64, Vec<u32>) {
        let mut referenced = 0u64;
        let mut counts = Vec::with_capacity(self.external_rows.len());
        for (&table, &rows) in &self.external_rows {
            referenced |= 1u64 << table;
            counts.push(rows);
        }
        (referenced, counts)
    }

    /// The row count that sizes a column indexing `table`: the rows present here or the rows it
    /// has in another module, whichever is larger -- the count a reader sizes the column from.
    fn sizing_row_count(&self, table: u8) -> u32 {
        let external = self.external_rows.get(&table).copied().unwrap_or(0);
        self.row_count(table).max(external)
    }

    /// Serializes the `#~` stream: the header then every present table's rows,
    /// with column widths chosen from `heaps` and the row counts.
    #[must_use]
    pub fn serialize(&self, heaps: HeapSizes) -> Vec<u8> {
        let mut valid = 0u64;
        for (&table, rows) in &self.rows {
            if !rows.is_empty() {
                valid |= 1u64 << table;
            }
        }

        let mut out = Vec::new();
        out.extend_from_slice(&0u32.to_le_bytes());
        out.push(2);
        out.push(0);
        out.push(heaps.flags());
        out.push(1);
        out.extend_from_slice(&valid.to_le_bytes());
        out.extend_from_slice(&(self.sorted & valid).to_le_bytes());

        for table in 0u8..64 {
            if valid & (1u64 << table) != 0 {
                out.extend_from_slice(&self.row_count(table).to_le_bytes());
            }
        }
        for table in 0u8..64 {
            if valid & (1u64 << table) != 0 {
                for row in &self.rows[&table] {
                    for column in row {
                        self.write_column(column, heaps, &mut out);
                    }
                }
            }
        }
        out
    }

    fn write_column(&self, column: &Column, heaps: HeapSizes, out: &mut Vec<u8>) {
        match column {
            Column::U16(value) => out.extend_from_slice(&value.to_le_bytes()),
            Column::U32(value) => out.extend_from_slice(&value.to_le_bytes()),
            Column::StringRef(offset) => write_heap_ref(*offset, heaps.wide_strings, out),
            Column::GuidRef(index) => write_heap_ref(*index, heaps.wide_guid, out),
            Column::BlobRef(offset) => write_heap_ref(*offset, heaps.wide_blob, out),
            Column::Index(table, row) => {
                write_ref(*row, self.sizing_row_count(*table) >= 0x1_0000, out);
            }
            Column::Coded(kind, token) => {
                let wide = kind.width(|table| self.sizing_row_count(table)) == 4;
                write_ref(kind.encode(*token), wide, out);
            }
        }
    }
}

/// Writes one heap reference, 4 bytes when `wide` and 2 otherwise.
///
/// # Panics
/// When a reference above 0xFFFF is to be written narrow. Its high bits would be dropped and it
/// would name a different heap entry: a structurally valid image with the wrong names, signatures
/// or constants in it. Widths chosen by [`HeapSizes::for_heaps`] from the heaps being written never
/// ask for that.
fn write_heap_ref(value: u32, wide: bool, out: &mut Vec<u8>) {
    if !wide {
        assert!(
            value <= 0xFFFF,
            "heap reference {value:#x} does not fit the 2-byte width its heap was given"
        );
    }
    write_ref(value, wide, out);
}

fn write_ref(value: u32, wide: bool, out: &mut Vec<u8>) {
    if wide {
        out.extend_from_slice(&value.to_le_bytes());
    } else {
        out.extend_from_slice(&(value as u16).to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lamella_metadata::tables::table;

    fn u64_at(bytes: &[u8], offset: usize) -> u64 {
        u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
    }
    fn u32_at(bytes: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
    }

    #[test]
    fn an_empty_stream_is_just_the_header() {
        let stream = TableStream::new().serialize(HeapSizes::default());
        assert_eq!(stream.len(), 24);
        assert_eq!(stream[4], 2);
        assert_eq!(stream[7], 1);
        assert_eq!(u64_at(&stream, 8), 0);
    }

    #[test]
    fn a_module_row_sets_its_valid_bit_and_count() {
        let mut tables = TableStream::new();
        let row = tables.add_row(table::MODULE, module_row(1));
        assert_eq!(row, 1);

        let stream = tables.serialize(HeapSizes::default());
        assert_eq!(u64_at(&stream, 8), 1);
        assert_eq!(u32_at(&stream, 24), 1);
        assert_eq!(stream.len(), 24 + 4 + 10);
    }

    fn module_row(name: u32) -> Vec<Column> {
        alloc::vec![
            Column::U16(0),
            Column::StringRef(name),
            Column::GuidRef(1),
            Column::GuidRef(0),
            Column::GuidRef(0),
        ]
    }

    #[test]
    fn a_heap_of_0x10000_bytes_or_more_is_referenced_wide() {
        let under = alloc::vec![0u8; HeapSizes::WIDE_FROM - 1];
        let at = alloc::vec![0u8; HeapSizes::WIDE_FROM];
        assert_eq!(HeapSizes::for_heaps(&under, &under, &under).flags(), 0);
        assert_eq!(HeapSizes::for_heaps(&at, &under, &under).flags(), 0x01);
        assert_eq!(HeapSizes::for_heaps(&under, &at, &under).flags(), 0x02);
        assert_eq!(HeapSizes::for_heaps(&under, &under, &at).flags(), 0x04);
        assert_eq!(HeapSizes::for_heaps(&at, &at, &at).flags(), 0x07);
    }

    #[test]
    fn a_wide_heap_reference_takes_four_bytes() {
        let mut tables = TableStream::new();
        tables.add_row(table::MODULE, module_row(0x1_0001));
        let heaps = HeapSizes {
            wide_strings: true,
            ..HeapSizes::default()
        };
        let stream = tables.serialize(heaps);
        assert_eq!(stream[6], 0x01);
        assert_eq!(u32_at(&stream, 30), 0x1_0001);
        assert_eq!(stream.len(), 24 + 4 + 2 + 4 + 2 + 2 + 2);
    }

    #[test]
    #[should_panic(expected = "does not fit the 2-byte width")]
    fn a_heap_reference_too_large_for_its_width_is_refused() {
        let mut tables = TableStream::new();
        tables.add_row(table::MODULE, module_row(0x1_0001));
        let _ = tables.serialize(HeapSizes::default());
    }

    #[test]
    fn an_index_into_another_modules_table_is_sized_by_that_modules_row_count() {
        let mut tables = TableStream::new();
        tables.add_row(
            table::LOCAL_SCOPE,
            alloc::vec![Column::Index(table::METHOD_DEF, 0x1_0000)],
        );
        tables.set_external_row_count(table::METHOD_DEF, 0x1_0000);
        let stream = tables.serialize(HeapSizes::default());
        assert_eq!(u64_at(&stream, 8), 1u64 << table::LOCAL_SCOPE);
        assert_eq!(u32_at(&stream, 28), 0x1_0000);
        assert_eq!(stream.len(), 24 + 4 + 4);
        assert_eq!(
            tables.external_tables(),
            (1u64 << table::METHOD_DEF, alloc::vec![0x1_0000])
        );
    }

    #[test]
    fn a_coded_index_column_encodes_and_sizes() {
        let mut tables = TableStream::new();
        tables.add_row(
            table::TYPE_DEF,
            alloc::vec![Column::Coded(
                CodedIndex::TypeDefOrRef,
                Token::new(table::TYPE_REF, 1),
            )],
        );
        let stream = tables.serialize(HeapSizes::default());
        let row_start = stream.len() - 2;
        assert_eq!(
            u16::from_le_bytes([stream[row_start], stream[row_start + 1]]),
            5
        );
    }

    #[test]
    fn method_semantics_sorts_by_its_association_column() {
        let semantics_row = |method: u32, association: Token| {
            alloc::vec![
                Column::U16(0x2),
                Column::Index(table::METHOD_DEF, method),
                Column::Coded(CodedIndex::HasSemantics, association),
            ]
        };
        let property = Token::new(table::PROPERTY, 1);
        let event = Token::new(table::EVENT, 1);

        let mut descending = TableStream::new();
        descending.add_row(table::METHOD_SEMANTICS, semantics_row(1, property));
        descending.add_row(table::METHOD_SEMANTICS, semantics_row(2, event));
        descending.sort_by_coded_column(table::METHOD_SEMANTICS, 2);

        let mut ascending = TableStream::new();
        ascending.add_row(table::METHOD_SEMANTICS, semantics_row(2, event));
        ascending.add_row(table::METHOD_SEMANTICS, semantics_row(1, property));
        ascending.sort_by_coded_column(table::METHOD_SEMANTICS, 2);

        let sorted = descending.serialize(HeapSizes::default());
        assert_eq!(sorted, ascending.serialize(HeapSizes::default()));
        assert_eq!(
            u64_at(&sorted, 16) & (1u64 << table::METHOD_SEMANTICS),
            1u64 << table::METHOD_SEMANTICS
        );
    }

    #[test]
    fn generic_param_sorts_by_owner_and_keeps_number_order_within_one_owner() {
        let param = |number: u16, owner: Token| {
            alloc::vec![
                Column::U16(number),
                Column::U16(0),
                Column::Coded(CodedIndex::TypeOrMethodDef, owner),
                Column::StringRef(0),
            ]
        };
        let ty = Token::new(table::TYPE_DEF, 1);
        let method = Token::new(table::METHOD_DEF, 1);

        let mut descending = TableStream::new();
        descending.add_row(table::GENERIC_PARAM, param(0, method));
        descending.add_row(table::GENERIC_PARAM, param(0, ty));
        descending.add_row(table::GENERIC_PARAM, param(1, ty));
        descending.sort_by_coded_column(table::GENERIC_PARAM, 2);

        let mut ascending = TableStream::new();
        ascending.add_row(table::GENERIC_PARAM, param(0, ty));
        ascending.add_row(table::GENERIC_PARAM, param(1, ty));
        ascending.add_row(table::GENERIC_PARAM, param(0, method));
        ascending.sort_by_coded_column(table::GENERIC_PARAM, 2);

        let sorted = descending.serialize(HeapSizes::default());
        assert_eq!(sorted, ascending.serialize(HeapSizes::default()));
        assert_eq!(
            u64_at(&sorted, 16) & (1u64 << table::GENERIC_PARAM),
            1u64 << table::GENERIC_PARAM
        );

        let mut one_owner = TableStream::new();
        one_owner.add_row(table::GENERIC_PARAM, param(0, ty));
        one_owner.add_row(table::GENERIC_PARAM, param(1, ty));
        one_owner.add_row(table::GENERIC_PARAM, param(2, ty));
        let before = one_owner.serialize(HeapSizes::default());
        one_owner.sort_by_coded_column(table::GENERIC_PARAM, 2);
        let after = one_owner.serialize(HeapSizes::default());
        assert_eq!(before[24..], after[24..]);
    }
}
