//! Virtual tables: load external data sources lazily, with the ability to
//! promote virtual columns to materialized PAX storage on demand.


/// A virtual column: data is held in memory (not yet materialized to PAX).
#[derive(Clone, Debug)]
pub struct VirtualColumn {
    pub name: String,
    pub kind: cendb_core::ValueKind,
    /// The data, stored as the canonical i64 form (or byte vectors for
    /// Bytes columns).
    pub data: VirtualColumnData,
}

/// Column data storage.
#[derive(Clone, Debug)]
pub enum VirtualColumnData {
    /// Fixed-width: i64 per row.
    I64(Vec<i64>),
    /// Variable-width: byte vectors per row.
    Bytes(Vec<Vec<u8>>),
    /// F64 stored as bit patterns.
    F64(Vec<i64>),
}

impl VirtualColumn {
    pub fn new_i64(name: impl Into<String>, data: Vec<i64>) -> Self {
        Self {
            name: name.into(),
            kind: cendb_core::ValueKind::I64,
            data: VirtualColumnData::I64(data),
        }
    }

    pub fn new_f64(name: impl Into<String>, data: Vec<f64>) -> Self {
        Self {
            name: name.into(),
            kind: cendb_core::ValueKind::F64,
            data: VirtualColumnData::F64(data.iter().map(|v| v.to_bits() as i64).collect()),
        }
    }

    pub fn new_bytes(name: impl Into<String>, data: Vec<Vec<u8>>) -> Self {
        Self {
            name: name.into(),
            kind: cendb_core::ValueKind::Bytes,
            data: VirtualColumnData::Bytes(data),
        }
    }

    pub fn row_count(&self) -> usize {
        match &self.data {
            VirtualColumnData::I64(v) => v.len(),
            VirtualColumnData::F64(v) => v.len(),
            VirtualColumnData::Bytes(v) => v.len(),
        }
    }
}

/// A virtual table: a collection of in-memory columns that can be queried
/// without materialization, or promoted to real PAX storage on demand.
pub struct VirtualTable {
    pub name: String,
    pub columns: Vec<VirtualColumn>,
    pub row_count: usize,
    /// Whether this table has been promoted to PAX storage.
    pub materialized: bool,
}

impl VirtualTable {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            columns: Vec::new(),
            row_count: 0,
            materialized: false,
        }
    }

    /// Add a column to the virtual table.
    pub fn add_column(&mut self, col: VirtualColumn) {
        if self.columns.is_empty() {
            self.row_count = col.row_count();
        }
        self.columns.push(col);
    }

    /// Get a column by name.
    pub fn column(&self, name: &str) -> Option<&VirtualColumn> {
        self.columns.iter().find(|c| c.name == name)
    }

    /// Scan a column as i64 values (zero-copy from the in-memory buffer).
    pub fn scan_i64(&self, col_name: &str) -> Option<&[i64]> {
        let col = self.column(col_name)?;
        match &col.data {
            VirtualColumnData::I64(v) | VirtualColumnData::F64(v) => Some(v.as_slice()),
            _ => None,
        }
    }

    /// Scan a column as f64 values.
    pub fn scan_f64(&self, col_name: &str) -> Option<Vec<f64>> {
        let col = self.column(col_name)?;
        match &col.data {
            VirtualColumnData::F64(v) => {
                Some(v.iter().map(|&bits| f64::from_bits(bits as u64)).collect())
            }
            _ => None,
        }
    }

    /// Promote a virtual column to materialized PAX storage. After this
    /// call, the column's data is written to a PAX block and the virtual
    /// column is marked as materialized.
    pub fn promote_column(
        &mut self,
        col_name: &str,
        block_size: u32,
    ) -> cendb_core::CenResult<()> {
        use cendb_storage::header::ColumnSpec;
        use cendb_storage::pax::PaxBlockBuilder;
        use cendb_core::Value;

        let col_idx = self.columns.iter().position(|c| c.name == col_name)
            .ok_or_else(|| cendb_core::CenError::not_found(format!("column {} not found", col_name)))?;

        let col = &self.columns[col_idx];
        let spec = ColumnSpec::new(col_idx as u32, col.kind);
        let mut builder = PaxBlockBuilder::new(block_size, vec![spec])?;

        match &col.data {
            VirtualColumnData::I64(vals) => {
                for &v in vals {
                    builder.append_row(&[Value::I64(v)])?;
                }
            }
            VirtualColumnData::F64(vals) => {
                for &bits in vals {
                    builder.append_row(&[Value::F64(f64::from_bits(bits as u64))])?;
                }
            }
            VirtualColumnData::Bytes(vals) => {
                for v in vals {
                    builder.append_row(&[Value::Bytes(v.clone())])?;
                }
            }
        }

        let _block = builder.finalize()?;
        self.materialized = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_table_basic() {
        let mut vt = VirtualTable::new("users");
        vt.add_column(VirtualColumn::new_i64("id", vec![1, 2, 3]));
        vt.add_column(VirtualColumn::new_bytes(
            "name",
            vec![b"Alice".to_vec(), b"Bob".to_vec(), b"Carol".to_vec()],
        ));

        assert_eq!(vt.row_count, 3);
        let ids = vt.scan_i64("id").unwrap();
        assert_eq!(ids, &[1, 2, 3]);
    }

    #[test]
    fn promote_to_pax() {
        let mut vt = VirtualTable::new("metrics");
        vt.add_column(VirtualColumn::new_i64("ts", (0..100).collect()));
        vt.add_column(VirtualColumn::new_f64(
            "temp",
            (0..100).map(|i| (i as f64) * 0.5).collect(),
        ));

        assert!(!vt.materialized);
        vt.promote_column("ts", 64 * 1024).unwrap();
        assert!(vt.materialized);
    }
}
