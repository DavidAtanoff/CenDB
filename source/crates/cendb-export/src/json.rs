//! JSON export: convert rows to JSON or NDJSON format.

use cendb_core::Value;

/// Export rows as a JSON array.
///
/// ```json
/// [{"id": 1, "name": "Alice"}, {"id": 2, "name": "Bob"}]
/// ```
pub fn export_json(rows: &[Vec<Value>], column_names: &[String]) -> String {
    let mut out = String::with_capacity(rows.len() * 64);
    out.push('[');
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('{');
        for (j, val) in row.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            if let Some(name) = column_names.get(j) {
                out.push('"');
                out.push_str(&escape_json(name));
                out.push_str("\":");
            }
            out.push_str(&value_to_json(val));
        }
        out.push('}');
    }
    out.push(']');
    out
}

/// Export rows as NDJSON (one JSON object per line).
///
/// ```json
/// {"id": 1, "name": "Alice"}
/// {"id": 2, "name": "Bob"}
/// ```
pub fn export_ndjson(rows: &[Vec<Value>], column_names: &[String]) -> String {
    let mut out = String::with_capacity(rows.len() * 64);
    for row in rows {
        out.push('{');
        for (j, val) in row.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            if let Some(name) = column_names.get(j) {
                out.push('"');
                out.push_str(&escape_json(name));
                out.push_str("\":");
            }
            out.push_str(&value_to_json(val));
        }
        out.push_str("}\n");
    }
    out
}

fn value_to_json(val: &Value) -> String {
    match val {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::I64(n) => n.to_string(),
        Value::U64(n) => n.to_string(),
        Value::F64(x) => {
            if x.is_finite() {
                x.to_string()
            } else {
                "null".to_string()
            }
        }
        Value::Bytes(b) => {
            let s = String::from_utf8_lossy(b);
            format!("\"{}\"", escape_json(&s))
        }
        Value::Timestamp(ts) => ts.to_string(),
    }
}

fn escape_json(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < ' ' => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_json_basic() {
        let rows = vec![
            vec![Value::I64(1), Value::Bytes(b"Alice".to_vec())],
            vec![Value::I64(2), Value::Bytes(b"Bob".to_vec())],
        ];
        let cols = vec!["id".to_string(), "name".to_string()];
        let json = export_json(&rows, &cols);
        assert!(json.contains("\"id\":1"));
        assert!(json.contains("\"name\":\"Alice\""));
        assert!(json.contains("\"name\":\"Bob\""));
    }

    #[test]
    fn export_ndjson_format() {
        let rows = vec![
            vec![Value::I64(1)],
            vec![Value::I64(2)],
        ];
        let cols = vec!["id".to_string()];
        let ndjson = export_ndjson(&rows, &cols);
        let lines: Vec<&str> = ndjson.trim().lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"id\":1"));
    }

    #[test]
    fn escape_special_chars() {
        let rows = vec![vec![Value::Bytes(b"hello\"world\n".to_vec())]];
        let cols = vec!["text".to_string()];
        let json = export_json(&rows, &cols);
        assert!(json.contains("\\\""));
        assert!(json.contains("\\n"));
    }
}
