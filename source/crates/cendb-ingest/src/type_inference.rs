//! Automatic type inference for CSV columns.
//!
//! Scans sample rows and infers the most specific type that fits all
//! values: Null → Bool → I64 → F64 → String.

use crate::csv::CsvParser;

/// Inferred column type, ordered from most specific to least.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum InferredType {
    /// All values are empty/null.
    Null,
    /// Values are "true" or "false".
    Bool,
    /// Values parse as integers.
    I64,
    /// Values parse as floats (but not integers).
    F64,
    /// Values are arbitrary strings.
    String,
}

impl InferredType {
    /// Promote this type to accommodate `other` (take the more general).
    pub fn merge(self, other: InferredType) -> InferredType {
        use InferredType::*;
        match (self, other) {
            (Null, x) | (x, Null) => x,
            (Bool, Bool) => Bool,
            (Bool, _) | (_, Bool) => String, // bool doesn't merge with numbers
            (I64, I64) => I64,
            (I64, F64) | (F64, I64) | (F64, F64) => F64,
            (String, _) | (_, String) => String,
        }
    }

    /// Convert to a CenDB ValueKind.
    pub fn to_value_kind(self) -> cendb_core::ValueKind {
        match self {
            InferredType::Null => cendb_core::ValueKind::Null,
            InferredType::Bool => cendb_core::ValueKind::Bool,
            InferredType::I64 => cendb_core::ValueKind::I64,
            InferredType::F64 => cendb_core::ValueKind::F64,
            InferredType::String => cendb_core::ValueKind::Bytes,
        }
    }
}

/// Infer column types from a CSV input. Scans up to `sample_size` rows.
pub fn infer_column_types(input: &[u8], sample_size: usize) -> Vec<InferredType> {
    let mut parser = CsvParser::new(input);
    let header = parser.parse_header();

    if header.is_none() {
        // No header — try to parse first row to get column count.
        let first = parser.next_record();
        if first.is_none() {
            return Vec::new();
        }
        let col_count = first.as_ref().unwrap().field_count();
        // Re-create parser since we consumed the first row.
        let mut parser2 = CsvParser::new(input).no_header();
        return infer_types_from_parser(&mut parser2, col_count, sample_size);
    }

    let col_count = header.as_ref().unwrap().field_count();
    infer_types_from_parser(&mut parser, col_count, sample_size)
}

fn infer_types_from_parser(
    parser: &mut CsvParser,
    col_count: usize,
    sample_size: usize,
) -> Vec<InferredType> {
    let mut types = vec![InferredType::Null; col_count];

    for _ in 0..sample_size {
        match parser.next_record() {
            Some(record) => {
                for (i, field) in record.fields.iter().enumerate() {
                    if i >= types.len() {
                        break;
                    }
                    let field_type = infer_field_type(field);
                    types[i] = types[i].merge(field_type);
                }
            }
            None => break,
        }
    }

    types
}

fn infer_field_type(field: &[u8]) -> InferredType {
    if field.is_empty() {
        return InferredType::Null;
    }

    // Check bool.
    if field == b"true" || field == b"false" || field == b"True" || field == b"False" {
        return InferredType::Bool;
    }

    // Check integer.
    if std::str::from_utf8(field)
        .ok()
        .map(|s| s.parse::<i64>().is_ok())
        .unwrap_or(false)
    {
        return InferredType::I64;
    }

    // Check float.
    if std::str::from_utf8(field)
        .ok()
        .map(|s| s.parse::<f64>().is_ok())
        .unwrap_or(false)
    {
        return InferredType::F64;
    }

    InferredType::String
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infer_mixed_types() {
        let input = b"name,age,score,active\nAlice,30,95.5,true\nBob,25,87.3,false\nCharlie,40,100.0,true";
        let types = infer_column_types(input, 100);
        assert_eq!(types.len(), 4);
        assert_eq!(types[0], InferredType::String);
        assert_eq!(types[1], InferredType::I64);
        assert_eq!(types[2], InferredType::F64);
        assert_eq!(types[3], InferredType::Bool);
    }

    #[test]
    fn infer_all_integers() {
        let input = b"a,b\n1,2\n3,4\n5,6";
        let types = infer_column_types(input, 100);
        assert_eq!(types[0], InferredType::I64);
        assert_eq!(types[1], InferredType::I64);
    }

    #[test]
    fn infer_null_column() {
        let input = b"a,b\n1,\n2,\n3,";
        let types = infer_column_types(input, 100);
        assert_eq!(types[1], InferredType::Null);
    }

    #[test]
    fn type_merge_promotes_to_float() {
        let t = InferredType::I64.merge(InferredType::F64);
        assert_eq!(t, InferredType::F64);
    }
}
