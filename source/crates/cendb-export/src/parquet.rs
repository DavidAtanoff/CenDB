//! Parquet integration: read/write CenDB data in Parquet format.
//!
//! Uses the official `parquet` crate for full Parquet format compliance.
//! Supports columnar read/write with Snappy compression by default.

use arrow::record_batch::RecordBatch;
use parquet::arrow::arrow_writer::ArrowWriter;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::file::properties::WriterProperties;
use parquet::basic::Compression;
use bytes::Bytes;
use std::sync::Arc;

use crate::arrow::{ArrowTable, ColumnData};

/// Parquet write options.
#[derive(Clone, Debug)]
pub struct ParquetWriteOptions {
    /// Compression codec (default: SNAPPY for best speed/ratio balance).
    pub compression: ParquetCompression,
    /// Row group size (default: 1M rows per group).
    pub row_group_size: usize,
    /// Enable dictionary encoding for string/binary columns.
    pub enable_dictionary: bool,
    /// Enable statistics (min/max per column chunk).
    pub enable_statistics: bool,
}

impl Default for ParquetWriteOptions {
    fn default() -> Self {
        Self {
            compression: ParquetCompression::Snappy,
            row_group_size: 1_000_000,
            enable_dictionary: true,
            enable_statistics: true,
        }
    }
}

/// Parquet compression codecs.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ParquetCompression {
    Uncompressed,
    Snappy,
    Gzip,
    Lzo,
    Brotli,
    Lz4,
    Zstd,
}

impl ParquetCompression {
    fn to_parquet(self) -> Compression {
        match self {
            ParquetCompression::Uncompressed => Compression::UNCOMPRESSED,
            ParquetCompression::Snappy => Compression::SNAPPY,
            ParquetCompression::Gzip => Compression::GZIP(Default::default()),
            ParquetCompression::Lzo => Compression::LZO,
            ParquetCompression::Brotli => Compression::BROTLI(Default::default()),
            ParquetCompression::Lz4 => Compression::LZ4,
            ParquetCompression::Zstd => Compression::ZSTD(Default::default()),
        }
    }
}

/// Write a CenDB table to Parquet bytes.
pub fn write_parquet(table: &ArrowTable, options: &ParquetWriteOptions) -> parquet::errors::Result<Vec<u8>> {
    let batch = table.to_record_batch().map_err(|e| {
        parquet::errors::ParquetError::ArrowError(e.to_string())
    })?;

    use parquet::file::properties::EnabledStatistics;

    let mut props_builder = WriterProperties::builder()
        .set_compression(options.compression.to_parquet())
        .set_dictionary_enabled(options.enable_dictionary)
        .set_statistics_enabled(if options.enable_statistics {
            EnabledStatistics::Page
        } else {
            EnabledStatistics::None
        });

    if options.row_group_size > 0 {
        props_builder = props_builder.set_max_row_group_row_count(Some(options.row_group_size));
    }

    let props = props_builder.build();

    let mut buf = Vec::new();
    let mut writer = ArrowWriter::try_new(&mut buf, batch.schema(), Some(props))?;
    writer.write(&batch)?;
    writer.close()?;
    Ok(buf)
}

/// Read a CenDB table from Parquet bytes.
pub fn read_parquet(bytes: &[u8]) -> parquet::errors::Result<ArrowTable> {
    let bytes_owned = Bytes::copy_from_slice(bytes);
    let reader = ParquetRecordBatchReaderBuilder::try_new(bytes_owned)?
        .with_batch_size(1_000_000) // read all rows in one batch
        .build()?;

    let mut tables = Vec::new();
    for batch in reader {
        let batch: RecordBatch = batch?;
        tables.push(ArrowTable::from_record_batch(&batch));
    }

    if tables.is_empty() {
        return Ok(ArrowTable::new());
    }

    if tables.len() == 1 {
        return Ok(tables.into_iter().next().unwrap());
    }

    // Multi-batch merge: concatenate all column data.
    let mut merged = ArrowTable::new();
    let num_cols = tables[0].columns.len();
    for col_idx in 0..num_cols {
        let name = tables[0].columns[col_idx].0.clone();
        // Concatenate data across all batches.
        let mut all_data: Vec<ColumnData> = Vec::new();
        for t in &tables {
            all_data.push(t.columns[col_idx].1.clone());
        }
        let merged_data = concatenate_column_data(&all_data);
        merged.add_column(&name, merged_data);
    }
    Ok(merged)
}

/// Concatenate multiple `ColumnData` into one.
fn concatenate_column_data(parts: &[ColumnData]) -> ColumnData {
    if parts.is_empty() {
        return ColumnData::Null(0);
    }
    match &parts[0] {
        ColumnData::Int64(_) => {
            let mut v = Vec::new();
            for p in parts {
                if let ColumnData::Int64(d) = p { v.extend_from_slice(d); }
            }
            ColumnData::Int64(v)
        }
        ColumnData::Float64(_) => {
            let mut v = Vec::new();
            for p in parts {
                if let ColumnData::Float64(d) = p { v.extend_from_slice(d); }
            }
            ColumnData::Float64(v)
        }
        ColumnData::Utf8(_) => {
            let mut v = Vec::new();
            for p in parts {
                if let ColumnData::Utf8(d) = p { v.extend(d.iter().cloned()); }
            }
            ColumnData::Utf8(v)
        }
        ColumnData::Boolean(_) => {
            let mut v = Vec::new();
            for p in parts {
                if let ColumnData::Boolean(d) = p { v.extend_from_slice(d); }
            }
            ColumnData::Boolean(v)
        }
        ColumnData::Binary(_) => {
            let mut v = Vec::new();
            for p in parts {
                if let ColumnData::Binary(d) = p { v.extend(d.iter().cloned()); }
            }
            ColumnData::Binary(v)
        }
        ColumnData::Null(n) => {
            let total: usize = parts.iter().map(|p| p.len()).sum();
            ColumnData::Null(total)
        }
    }
}

/// Write a CenDB table to a Parquet file on disk.
pub fn write_parquet_file(table: &ArrowTable, path: &str, options: &ParquetWriteOptions) -> parquet::errors::Result<()> {
    let bytes = write_parquet(table, options)?;
    std::fs::write(path, bytes).map_err(|e| {
        parquet::errors::ParquetError::General(format!("file write error: {}", e))
    })?;
    Ok(())
}

/// Read a CenDB table from a Parquet file on disk.
pub fn read_parquet_file(path: &str) -> parquet::errors::Result<ArrowTable> {
    let bytes = std::fs::read(path).map_err(|e| {
        parquet::errors::ParquetError::General(format!("file read error: {}", e))
    })?;
    read_parquet(&bytes)
}

/// Get Parquet file metadata (row count, column count, compressed size).
pub fn parquet_metadata(bytes: &[u8]) -> parquet::errors::Result<ParquetMetadata> {
    let bytes_owned = Bytes::copy_from_slice(bytes);
    let reader = ParquetRecordBatchReaderBuilder::try_new(bytes_owned)?;
    let metadata = reader.metadata();
    let row_count = metadata.file_metadata().num_rows() as usize;
    let num_cols = metadata.file_metadata().schema_descr().num_columns();
    let compressed_size = bytes.len();

    Ok(ParquetMetadata {
        row_count,
        num_cols,
        compressed_size,
        row_groups: metadata.num_row_groups(),
    })
}

/// Metadata about a Parquet file.
#[derive(Clone, Debug)]
pub struct ParquetMetadata {
    pub row_count: usize,
    pub num_cols: usize,
    pub compressed_size: usize,
    pub row_groups: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arrow::ColumnData;

    #[test]
    fn parquet_roundtrip_basic() {
        let mut table = ArrowTable::new();
        table.add_column("id", ColumnData::Int64(vec![1, 2, 3, 4, 5]));
        table.add_column("score", ColumnData::Float64(vec![95.5, 87.3, 100.0, 72.1, 88.9]));
        table.add_column("name", ColumnData::Utf8(vec![
            "Alice".into(), "Bob".into(), "Carol".into(), "Dave".into(), "Eve".into(),
        ]));

        let options = ParquetWriteOptions::default();
        let bytes = write_parquet(&table, &options).unwrap();
        assert!(!bytes.is_empty());
        assert!(bytes.len() > 4); // Parquet magic + data

        let back = read_parquet(&bytes).unwrap();
        assert_eq!(back.columns.len(), 3);
        assert_eq!(back.row_count(), 5);

        // Verify Int64 column.
        match &back.columns[0].1 {
            ColumnData::Int64(v) => assert_eq!(v, &[1, 2, 3, 4, 5]),
            _ => panic!("expected Int64"),
        }
    }

    #[test]
    fn parquet_with_snappy_compression() {
        let mut table = ArrowTable::new();
        table.add_column("x", ColumnData::Int64((0..10_000).collect()));

        let options = ParquetWriteOptions {
            compression: ParquetCompression::Snappy,
            ..Default::default()
        };
        let snappy_bytes = write_parquet(&table, &options).unwrap();

        let options_uncompressed = ParquetWriteOptions {
            compression: ParquetCompression::Uncompressed,
            ..Default::default()
        };
        let uncompressed_bytes = write_parquet(&table, &options_uncompressed).unwrap();

        // Snappy should be smaller for this data (sequential integers compress well).
        assert!(snappy_bytes.len() < uncompressed_bytes.len(),
            "snappy {} should be < uncompressed {}", snappy_bytes.len(), uncompressed_bytes.len());

        // Verify roundtrip.
        let back = read_parquet(&snappy_bytes).unwrap();
        assert_eq!(back.row_count(), 10_000);
    }

    #[test]
    fn parquet_with_zstd_compression() {
        let mut table = ArrowTable::new();
        table.add_column("x", ColumnData::Int64((0..10_000).collect()));

        let options = ParquetWriteOptions {
            compression: ParquetCompression::Zstd,
            ..Default::default()
        };
        let zstd_bytes = write_parquet(&table, &options).unwrap();
        assert!(!zstd_bytes.is_empty());

        let back = read_parquet(&zstd_bytes).unwrap();
        assert_eq!(back.row_count(), 10_000);
    }

    #[test]
    fn parquet_file_metadata() {
        let mut table = ArrowTable::new();
        table.add_column("id", ColumnData::Int64((0..1000).collect()));
        table.add_column("name", ColumnData::Utf8((0..1000).map(|i| format!("user_{}", i)).collect()));

        let options = ParquetWriteOptions::default();
        let bytes = write_parquet(&table, &options).unwrap();
        let meta = parquet_metadata(&bytes).unwrap();

        assert_eq!(meta.row_count, 1000);
        assert_eq!(meta.num_cols, 2);
        assert_eq!(meta.row_groups, 1); // 1000 rows fits in one group
        assert!(meta.compressed_size > 0);
    }

    #[test]
    fn parquet_boolean_column() {
        let mut table = ArrowTable::new();
        table.add_column("flag", ColumnData::Boolean(vec![true, false, true, false, true]));

        let bytes = write_parquet(&table, &ParquetWriteOptions::default()).unwrap();
        let back = read_parquet(&bytes).unwrap();
        assert_eq!(back.row_count(), 5);
        match &back.columns[0].1 {
            ColumnData::Boolean(v) => assert_eq!(v, &[true, false, true, false, true]),
            _ => panic!("expected Boolean"),
        }
    }

    #[test]
    fn parquet_binary_column() {
        let mut table = ArrowTable::new();
        table.add_column("blob", ColumnData::Binary(vec![
            b"\x00\x01\x02".to_vec(),
            b"\x03\x04\x05\x06".to_vec(),
            b"\x07".to_vec(),
        ]));

        let bytes = write_parquet(&table, &ParquetWriteOptions::default()).unwrap();
        let back = read_parquet(&bytes).unwrap();
        assert_eq!(back.row_count(), 3);
        match &back.columns[0].1 {
            ColumnData::Binary(v) => {
                assert_eq!(v.len(), 3);
                assert_eq!(v[0], b"\x00\x01\x02");
                assert_eq!(v[1], b"\x03\x04\x05\x06");
                assert_eq!(v[2], b"\x07");
            }
            _ => panic!("expected Binary"),
        }
    }

    #[test]
    fn parquet_dictionary_encoding() {
        // Low-cardinality string column should compress well with dictionary.
        let mut table = ArrowTable::new();
        let countries = vec!["US", "UK", "DE", "FR", "JP"];
        let values: Vec<String> = (0..10_000).map(|i| countries[i % 5].to_string()).collect();
        table.add_column("country", ColumnData::Utf8(values));

        let with_dict = ParquetWriteOptions {
            enable_dictionary: true,
            ..Default::default()
        };
        let without_dict = ParquetWriteOptions {
            enable_dictionary: false,
            ..Default::default()
        };

        let dict_bytes = write_parquet(&table, &with_dict).unwrap();
        let no_dict_bytes = write_parquet(&table, &without_dict).unwrap();

        // Dictionary encoding should produce a smaller file for low-cardinality data.
        assert!(dict_bytes.len() <= no_dict_bytes.len(),
            "dictionary {} should be <= no-dictionary {}", dict_bytes.len(), no_dict_bytes.len());
    }

    #[test]
    fn parquet_file_roundtrip() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("cendb_parquet_test_{}.parquet", std::process::id()));

        let mut table = ArrowTable::new();
        table.add_column("id", ColumnData::Int64(vec![1, 2, 3]));
        table.add_column("name", ColumnData::Utf8(vec!["a".into(), "b".into(), "c".into()]));

        write_parquet_file(&table, path.to_str().unwrap(), &ParquetWriteOptions::default()).unwrap();
        let back = read_parquet_file(path.to_str().unwrap()).unwrap();
        assert_eq!(back.row_count(), 3);

        std::fs::remove_file(&path).ok();
    }
}
