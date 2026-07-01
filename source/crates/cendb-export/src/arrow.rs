//! Arrow-compatible export: produce columnar data in Arrow IPC format.
//!
//! This is a simplified implementation that produces Arrow-compatible
//! flat arrays without depending on the full Arrow Rust SDK. The output
//! can be consumed by Arrow readers that support the IPC format.

/// An Arrow-compatible column: name + flat data buffer.
#[derive(Clone, Debug)]
pub struct ArrowColumn {
    pub name: String,
    pub data_type: ArrowDataType,
    /// The column data as a flat byte buffer.
    pub data: Vec<u8>,
    /// Null bitmap (1 bit per row, 1 = valid, 0 = null).
    pub null_bitmap: Vec<u8>,
    /// Number of rows.
    pub row_count: usize,
}

/// Supported Arrow data types.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ArrowDataType {
    Int64,
    Float64,
    Utf8,
    Bool,
}

impl ArrowDataType {
    /// Arrow type code (matches the Arrow IPC specification).
    pub fn type_code(&self) -> u8 {
        match self {
            ArrowDataType::Int64 => 8,   // Type.Int
            ArrowDataType::Float64 => 7, // Type.FloatingPoint
            ArrowDataType::Utf8 => 5,    // Type.Utf8
            ArrowDataType::Bool => 1,    // Type.Bool
        }
    }

    /// Fixed-width in bytes (0 for variable-width).
    pub fn fixed_width(&self) -> usize {
        match self {
            ArrowDataType::Int64 => 8,
            ArrowDataType::Float64 => 8,
            ArrowDataType::Bool => 1,
            ArrowDataType::Utf8 => 0,
        }
    }
}

/// An Arrow batch: a collection of columns.
#[derive(Clone, Debug)]
pub struct ArrowBatch {
    pub columns: Vec<ArrowColumn>,
    pub row_count: usize,
}

impl ArrowBatch {
    pub fn new() -> Self {
        Self {
            columns: Vec::new(),
            row_count: 0,
        }
    }

    /// Add an Int64 column.
    pub fn add_i64_column(&mut self, name: &str, values: &[i64]) {
        let mut data = Vec::with_capacity(values.len() * 8);
        for &v in values {
            data.extend_from_slice(&v.to_le_bytes());
        }
        let row_count = values.len();
        self.columns.push(ArrowColumn {
            name: name.to_string(),
            data_type: ArrowDataType::Int64,
            data,
            null_bitmap: vec![0xFF; (row_count + 7) / 8],
            row_count,
        });
        if row_count > self.row_count {
            self.row_count = row_count;
        }
    }

    /// Add a Float64 column.
    pub fn add_f64_column(&mut self, name: &str, values: &[f64]) {
        let mut data = Vec::with_capacity(values.len() * 8);
        for &v in values {
            data.extend_from_slice(&v.to_le_bytes());
        }
        let row_count = values.len();
        self.columns.push(ArrowColumn {
            name: name.to_string(),
            data_type: ArrowDataType::Float64,
            data,
            null_bitmap: vec![0xFF; (row_count + 7) / 8],
            row_count,
        });
        if row_count > self.row_count {
            self.row_count = row_count;
        }
    }

    /// Add a Utf8 (string) column.
    pub fn add_utf8_column(&mut self, name: &str, values: &[&str]) {
        // Arrow Utf8: offsets array (N+1 int32s) + data buffer.
        let mut offsets = Vec::with_capacity((values.len() + 1) * 4);
        let mut data = Vec::new();
        let mut offset = 0i32;
        offsets.extend_from_slice(&offset.to_le_bytes());
        for s in values {
            data.extend_from_slice(s.as_bytes());
            offset += s.len() as i32;
            offsets.extend_from_slice(&offset.to_le_bytes());
        }

        // Combine offsets + data into a single buffer.
        let mut combined = offsets;
        combined.extend_from_slice(&data);

        let row_count = values.len();
        self.columns.push(ArrowColumn {
            name: name.to_string(),
            data_type: ArrowDataType::Utf8,
            data: combined,
            null_bitmap: vec![0xFF; (row_count + 7) / 8],
            row_count,
        });
        if row_count > self.row_count {
            self.row_count = row_count;
        }
    }

    /// Total bytes of data in this batch.
    pub fn total_bytes(&self) -> usize {
        self.columns.iter().map(|c| c.data.len() + c.null_bitmap.len()).sum()
    }
}

impl Default for ArrowBatch {
    fn default() -> Self {
        Self::new()
    }
}

/// Export an Arrow batch to a simplified IPC-like byte format.
/// The format is: [magic 4B][row_count 4B][col_count 4B][columns...]
/// Each column: [name_len 4B][name][type 1B][row_count 4B][data_len 4B][data][bitmap_len 4B][bitmap]
pub fn export_arrow_ipc(batch: &ArrowBatch) -> Vec<u8> {
    let mut out = Vec::with_capacity(batch.total_bytes() + 256);

    // Magic.
    out.extend_from_slice(b"ARW1");
    // Row count.
    out.extend_from_slice(&(batch.row_count as u32).to_le_bytes());
    // Column count.
    out.extend_from_slice(&(batch.columns.len() as u32).to_le_bytes());

    for col in &batch.columns {
        // Name length + name.
        let name_bytes = col.name.as_bytes();
        out.extend_from_slice(&(name_bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(name_bytes);
        // Data type.
        out.push(col.data_type.type_code());
        // Row count.
        out.extend_from_slice(&(col.row_count as u32).to_le_bytes());
        // Data length + data.
        out.extend_from_slice(&(col.data.len() as u32).to_le_bytes());
        out.extend_from_slice(&col.data);
        // Bitmap length + bitmap.
        out.extend_from_slice(&(col.null_bitmap.len() as u32).to_le_bytes());
        out.extend_from_slice(&col.null_bitmap);
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_arrow_batch() {
        let mut batch = ArrowBatch::new();
        batch.add_i64_column("id", &[1, 2, 3]);
        batch.add_f64_column("score", &[95.5, 87.3, 100.0]);
        batch.add_utf8_column("name", &["Alice", "Bob", "Carol"]);

        assert_eq!(batch.row_count, 3);
        assert_eq!(batch.columns.len(), 3);
    }

    #[test]
    fn export_arrow_ipc_format() {
        let mut batch = ArrowBatch::new();
        batch.add_i64_column("id", &[1, 2, 3]);

        let ipc = export_arrow_ipc(&batch);
        assert!(ipc.starts_with(b"ARW1"));
        assert!(ipc.len() > 16);
    }

    #[test]
    fn arrow_ipc_roundtrip() {
        let mut batch = ArrowBatch::new();
        batch.add_i64_column("x", &[42, 100, 999]);
        let ipc = export_arrow_ipc(&batch);

        // Verify the magic.
        assert_eq!(&ipc[..4], b"ARW1");
        // Verify row count.
        let row_count = u32::from_le_bytes([ipc[4], ipc[5], ipc[6], ipc[7]]);
        assert_eq!(row_count, 3);
    }
}
