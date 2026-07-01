//! Relational projection: schema-bound tuples stored as PAX blocks.
//!
//! A "table" in this projection is just a sequence of PAX blocks sharing the
//! same column schema. Point lookups by primary key use the in-memory
//! [`RelationalIndex`]; range scans stream over blocks and decode columns
//! lazily.

use std::collections::HashMap;

use cendb_core::{BlockId, CenError, CenResult, SegmentId, Value, ValueKind};
use cendb_storage::header::ColumnSpec;
use cendb_storage::pax::{PaxBlock, PaxBlockBuilder};

/// Schema of a relational table: ordered list of column specs.
#[derive(Clone, Debug)]
pub struct TableSchema {
    pub name: String,
    pub columns: Vec<ColumnSpec>,
}

impl TableSchema {
    pub fn new(name: impl Into<String>, columns: Vec<ColumnSpec>) -> Self {
        Self { name: name.into(), columns }
    }

    /// Find the index of the column with `col_id`.
    pub fn find_column(&self, col_id: u32) -> Option<usize> {
        self.columns.iter().position(|c| c.col_id == col_id)
    }

    /// Find the index of the PK column, if any.
    pub fn pk_index(&self) -> Option<usize> {
        self.columns.iter().position(|c| c.is_pk != 0)
    }
}

/// Index entry: primary key value → (block_id, slot_id).
type RelationalIndex = HashMap<i64, (BlockId, u32)>;

/// A relational table: schema + sealed blocks + PK index.
pub struct RelationalTable {
    schema: TableSchema,
    /// Reserved for future use (will be used when the table spills to a
    /// real segment file on disk).
    #[allow(dead_code)]
    segment_id: SegmentId,
    block_size: u32,
    blocks: Vec<PaxBlock>,
    index: RelationalIndex,
    pending: Vec<Vec<Value>>,
    pending_capacity: usize,
}

impl RelationalTable {
    pub fn new(schema: TableSchema, segment_id: SegmentId, block_size: u32) -> CenResult<Self> {
        if schema.columns.is_empty() {
            return Err(CenError::constraint("RelationalTable: schema must have >= 1 column"));
        }
        if schema.pk_index().is_none() {
            return Err(CenError::constraint(
                "RelationalTable: schema must have a primary key column (mark one with .pk())",
            ));
        }
        Ok(Self {
            schema,
            segment_id,
            block_size,
            blocks: Vec::new(),
            index: HashMap::new(),
            pending: Vec::new(),
            pending_capacity: 512,
        })
    }

    pub fn schema(&self) -> &TableSchema {
        &self.schema
    }

    /// Insert a row. The row's values must be in the same order as the
    /// schema's columns.
    pub fn insert(&mut self, row: Vec<Value>) -> CenResult<()> {
        if row.len() != self.schema.columns.len() {
            return Err(CenError::constraint(format!(
                "insert: expected {} values, got {}",
                self.schema.columns.len(),
                row.len()
            )));
        }
        self.pending.push(row);
        if self.pending.len() >= self.pending_capacity {
            self.flush_pending()?;
        }
        Ok(())
    }

    /// Look up a row by primary key.
    pub fn find_by_pk(&self, pk: i64) -> CenResult<Option<Vec<Value>>> {
        // Check pending first.
        let pk_idx = self.schema.pk_index().unwrap();
        for row in self.pending.iter().rev() {
            if let Value::I64(v) = &row[pk_idx] {
                if *v == pk {
                    return Ok(Some(row.clone()));
                }
            }
        }
        // Check index.
        if let Some(&(block_id, slot)) = self.index.get(&pk) {
            let block = &self.blocks[block_id.0 as usize];
            return Ok(Some(block.materialize_row(slot as usize)?));
        }
        Ok(None)
    }

    /// Flush pending rows into a new sealed PAX block.
    pub fn flush_pending(&mut self) -> CenResult<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let mut builder = PaxBlockBuilder::new(self.block_size, self.schema.columns.clone())?;
        let mut new_entries: Vec<(i64, u32)> = Vec::new();
        let pk_idx = self.schema.pk_index().unwrap();
        for row in self.pending.drain(..) {
            let pk_val = match &row[pk_idx] {
                Value::I64(v) => *v,
                Value::U64(v) => *v as i64,
                _ => 0,
            };
            let row_id = builder.append_row(&row)?;
            new_entries.push((pk_val, row_id.0));
        }
        let block = builder.finalize()?;
        let block_id = BlockId(self.blocks.len() as u32);
        for (pk, slot) in new_entries {
            self.index.insert(pk, (block_id, slot));
        }
        self.blocks.push(block);
        Ok(())
    }

    /// Scan a single column across all sealed blocks. Returns the decoded
    /// `Vec<i64>` for the column at `col_idx`. Used by analytical queries
    /// that only touch a subset of columns (the "columnar projection"
    /// benefit of PAX).
    pub fn scan_column_i64(&self, col_idx: usize) -> CenResult<Vec<i64>> {
        let mut out = Vec::new();
        for block in &self.blocks {
            let vals = block.decode_i64_column(col_idx)?;
            out.extend(vals);
        }
        // Also include pending rows.
        for row in &self.pending {
            if let Value::I64(v) = &row[col_idx] {
                out.push(*v);
            } else if let Value::U64(v) = &row[col_idx] {
                out.push(*v as i64);
            } else if let Value::Bool(b) = &row[col_idx] {
                out.push(*b as i64);
            }
        }
        Ok(out)
    }

    /// Iterate over all rows (materialises each row — for analytical scans
    /// prefer [`scan_column_i64`]).
    pub fn iter(&self) -> RelationalIter<'_> {
        RelationalIter {
            table: self,
            pending_idx: 0,
            block_idx: 0,
            slot_idx: 0,
        }
    }

    pub fn row_count(&self) -> usize {
        self.pending.len() + self.blocks.iter().map(|b| b.header().row_count as usize).sum::<usize>()
    }

    pub fn block_count(&self) -> usize {
        self.blocks.len()
    }
}

pub struct RelationalIter<'a> {
    table: &'a RelationalTable,
    pending_idx: usize,
    block_idx: usize,
    slot_idx: u32,
}

impl<'a> Iterator for RelationalIter<'a> {
    type Item = CenResult<Vec<Value>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.pending_idx < self.table.pending.len() {
            let row = self.table.pending[self.pending_idx].clone();
            self.pending_idx += 1;
            return Some(Ok(row));
        }
        while self.block_idx < self.table.blocks.len() {
            let block = &self.table.blocks[self.block_idx];
            let hdr = block.header();
            if self.slot_idx >= hdr.row_count {
                self.block_idx += 1;
                self.slot_idx = 0;
                continue;
            }
            let slot = self.slot_idx as usize;
            self.slot_idx += 1;
            return Some(block.materialize_row(slot));
        }
        None
    }
}

/// Stateless projection helpers.
pub struct RelationalProjection;

impl RelationalProjection {
    /// Build a single PAX block from an iterator of rows.
    pub fn build_block<'a, I>(
        block_size: u32,
        schema: &TableSchema,
        rows: I,
    ) -> CenResult<PaxBlock>
    where
        I: IntoIterator<Item = &'a [Value]>,
    {
        let mut builder = PaxBlockBuilder::new(block_size, schema.columns.clone())?;
        for row in rows {
            builder.append_row(row)?;
        }
        builder.finalize()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_schema() -> TableSchema {
        TableSchema::new(
            "users",
            vec![
                ColumnSpec::new(0, ValueKind::I64).pk(),
                ColumnSpec::new(1, ValueKind::Bytes),
                ColumnSpec::new(2, ValueKind::I64),
            ],
        )
    }

    #[test]
    fn insert_and_find_by_pk() {
        let mut table = RelationalTable::new(user_schema(), SegmentId(1), 16 * 1024).unwrap();
        table
            .insert(vec![
                Value::I64(42),
                Value::Bytes(b"alice".to_vec()),
                Value::I64(30),
            ])
            .unwrap();
        table.flush_pending().unwrap();

        let row = table.find_by_pk(42).unwrap().unwrap();
        assert_eq!(row.len(), 3);
        match &row[1] {
            Value::Bytes(b) => assert_eq!(b, b"alice"),
            other => panic!("expected Bytes, got {:?}", other),
        }
    }

    #[test]
    fn scan_single_column() {
        let mut table = RelationalTable::new(user_schema(), SegmentId(1), 16 * 1024).unwrap();
        for i in 0..100i64 {
            table
                .insert(vec![
                    Value::I64(i),
                    Value::Bytes(format!("user-{}", i).into_bytes()),
                    Value::I64(i * 2),
                ])
                .unwrap();
        }
        table.flush_pending().unwrap();

        // Scan just the "age" column (col idx 2).
        let ages = table.scan_column_i64(2).unwrap();
        assert_eq!(ages.len(), 100);
        for (i, age) in ages.iter().enumerate() {
            assert_eq!(*age, (i as i64) * 2);
        }
    }
}
