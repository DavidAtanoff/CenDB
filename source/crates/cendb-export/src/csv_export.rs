//! CSV export: convert rows to comma-separated values.

use cendb_core::Value;

/// Export rows as CSV with an optional header.
pub fn export_csv(rows: &[Vec<Value>], column_names: Option<&[String]>) -> String {
    let mut out = String::with_capacity(rows.len() * 64);

    if let Some(names) = column_names {
        for (i, name) in names.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&escape_csv_field(name));
        }
        out.push('\n');
    }

    for row in rows {
        for (i, val) in row.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&escape_csv_field(&value_to_csv(val)));
        }
        out.push('\n');
    }

    out
}

fn value_to_csv(val: &Value) -> String {
    match val {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::I64(n) => n.to_string(),
        Value::U64(n) => n.to_string(),
        Value::F64(x) => x.to_string(),
        Value::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        Value::Timestamp(ts) => ts.to_string(),
    }
}

fn escape_csv_field(s: &str) -> String {
    let needs_quoting = s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r');
    if !needs_quoting {
        return s.to_string();
    }
    let escaped = s.replace('"', "\"\"");
    format!("\"{}\"", escaped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_csv_basic() {
        let rows = vec![
            vec![Value::I64(1), Value::Bytes(b"Alice".to_vec())],
            vec![Value::I64(2), Value::Bytes(b"Bob".to_vec())],
        ];
        let csv = export_csv(&rows, Some(&["id".into(), "name".into()]));
        assert!(csv.contains("id,name"));
        assert!(csv.contains("1,Alice"));
        assert!(csv.contains("2,Bob"));
    }

    #[test]
    fn export_csv_quoting() {
        let rows = vec![vec![Value::Bytes(b"Hello, World".to_vec())]];
        let csv = export_csv(&rows, None);
        assert!(csv.contains("\"Hello, World\""));
    }
}
