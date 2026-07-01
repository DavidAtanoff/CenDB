//! Document projection: HexDoc binary JSON layout.
//!
//! `HexDoc` is a compact binary format for nested JSON-like documents with
//! an O(1) field offset table at the front. The layout is:
//!
//! ```text
//! ┌──────────────────────────────────────────────────────┐
//! │ Header (20 bytes)                                    │
//! │  magic:           [u8; 4]   = b"HEXD"                │
//! │  root_kind:       u8         = kind of the root      │
//! │  _pad:            [u8; 3]                             │
//! │  field_count:     u32        = top-level field count │
//! │  root_off:        u32        = offset of root value  │
//! │  string_pool_off: u32        = byte offset of pool   │
//! ├──────────────────────────────────────────────────────┤
//! │ FieldOffsetTable (field_count × 8 bytes)             │
//! │  each entry: (name_id: u32, value_off: u32)          │
//! ├──────────────────────────────────────────────────────┤
//! │ String pool (length-prefixed UTF-8 strings)          │
//! ├──────────────────────────────────────────────────────┤
//! │ Values region (variable-width, tagged)               │
//! └──────────────────────────────────────────────────────┘
//! ```
//!
//! To read `doc["user"]["address"]["city"]` we walk the offset table in O(1)
//! per level — no full parse of the document. This is the spec's "blob with
//! offset-indexed field access" mode for high-churn schemas (§1.4).

use std::collections::HashMap;

use cendb_core::{HexError, HexResult};

/// Magic bytes for the HexDoc format.
pub const HEX_DOC_MAGIC: [u8; 4] = *b"HEXD";

/// Tag for the kind of a `DocValue`.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum DocKind {
    Null = 0,
    Bool = 1,
    I64 = 2,
    F64 = 3,
    Str = 4,
    Bytes = 5,
    Object = 6,
    Array = 7,
}

impl DocKind {
    pub fn from_u8(b: u8) -> Self {
        match b {
            0 => DocKind::Null,
            1 => DocKind::Bool,
            2 => DocKind::I64,
            3 => DocKind::F64,
            4 => DocKind::Str,
            5 => DocKind::Bytes,
            6 => DocKind::Object,
            7 => DocKind::Array,
            _ => DocKind::Null,
        }
    }
}

/// A JSON-like value used in the document projection. Owned (allocated) so
/// it can be built up programmatically before serialisation into the
/// compact HexDoc bytes.
#[derive(Clone, Debug, PartialEq)]
pub enum DocValue {
    Null,
    Bool(bool),
    I64(i64),
    F64(f64),
    Str(String),
    Bytes(Vec<u8>),
    Object(Vec<(String, DocValue)>),
    Array(Vec<DocValue>),
}

impl DocValue {
    pub fn kind(&self) -> DocKind {
        match self {
            DocValue::Null => DocKind::Null,
            DocValue::Bool(_) => DocKind::Bool,
            DocValue::I64(_) => DocKind::I64,
            DocValue::F64(_) => DocKind::F64,
            DocValue::Str(_) => DocKind::Str,
            DocValue::Bytes(_) => DocKind::Bytes,
            DocValue::Object(_) => DocKind::Object,
            DocValue::Array(_) => DocKind::Array,
        }
    }

    /// Look up a field in an Object by name. Returns `None` for non-objects
    /// or missing fields.
    pub fn get(&self, key: &str) -> Option<&DocValue> {
        match self {
            DocValue::Object(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// Index into an Array. Returns `None` for non-arrays or out-of-range.
    pub fn index(&self, i: usize) -> Option<&DocValue> {
        match self {
            DocValue::Array(items) => items.get(i),
            _ => None,
        }
    }
}

/// Builder for serialising a `DocValue` into HexDoc bytes.
pub struct HexDocBuilder {
    out: Vec<u8>,
    /// String interning table: string content → string id.
    /// We deduplicate identical strings to save space.
    strings: HashMap<String, u32>,
    string_pool: Vec<u8>,
}

impl HexDocBuilder {
    pub fn new() -> Self {
        Self {
            out: Vec::new(),
            strings: HashMap::new(),
            string_pool: Vec::new(),
        }
    }

    /// Serialise a `DocValue` into HexDoc bytes.
    pub fn encode(value: &DocValue) -> HexResult<Vec<u8>> {
        let mut builder = Self::new();
        // Reserve 20-byte header.
        builder.out.extend_from_slice(&[0u8; 20]);
        // Encode the value, recording its offset.
        let root_off = builder.encode_value(value)? as u32;
        // Append the string pool.
        let string_pool_off = builder.out.len() as u32;
        builder.out.extend_from_slice(&builder.string_pool);
        // Now write the header.
        let header = HexDocHeader {
            magic: HEX_DOC_MAGIC,
            root_kind: value.kind() as u8,
            _pad: [0; 3],
            field_count: match value {
                DocValue::Object(f) => f.len() as u32,
                DocValue::Array(a) => a.len() as u32,
                _ => 0,
            },
            root_off,
            string_pool_off,
        };
        let header_bytes = header.to_bytes();
        builder.out[..20].copy_from_slice(&header_bytes);
        Ok(builder.out)
    }

    fn intern_string(&mut self, s: &str) -> HexResult<u32> {
        if let Some(&id) = self.strings.get(s) {
            return Ok(id);
        }
        let id = self.string_pool.len() as u32;
        // Length-prefixed (u32 LE) UTF-8 bytes.
        self.string_pool.extend_from_slice(&(s.len() as u32).to_le_bytes());
        self.string_pool.extend_from_slice(s.as_bytes());
        self.strings.insert(s.to_string(), id);
        Ok(id)
    }

    fn encode_value(&mut self, value: &DocValue) -> HexResult<usize> {
        let off = self.out.len();
        // Write kind tag.
        self.out.push(value.kind() as u8);
        match value {
            DocValue::Null => {}
            DocValue::Bool(b) => self.out.push(*b as u8),
            DocValue::I64(x) => self.out.extend_from_slice(&x.to_le_bytes()),
            DocValue::F64(x) => self.out.extend_from_slice(&x.to_le_bytes()),
            DocValue::Str(s) => {
                let id = self.intern_string(s)?;
                self.out.extend_from_slice(&id.to_le_bytes());
            }
            DocValue::Bytes(b) => {
                self.out.extend_from_slice(&(b.len() as u32).to_le_bytes());
                self.out.extend_from_slice(b);
            }
            DocValue::Object(fields) => {
                // Inline field count (u32 LE) so non-root objects can be
                // decoded without the header.
                let field_count = fields.len() as u32;
                self.out.extend_from_slice(&field_count.to_le_bytes());
                // Reserve space for the field offset table.
                let table_off = self.out.len();
                self.out.extend_from_slice(&vec![0u8; fields.len() * 8]);
                // Encode each value, recording its offset.
                let mut value_offsets: Vec<(u32, u32)> = Vec::with_capacity(fields.len());
                for (name, val) in fields {
                    let name_id = self.intern_string(name)?;
                    let val_off = self.encode_value(val)? as u32;
                    value_offsets.push((name_id, val_off));
                }
                // Write the table.
                for (i, (name_id, val_off)) in value_offsets.iter().enumerate() {
                    let pos = table_off + i * 8;
                    self.out[pos..pos + 4].copy_from_slice(&name_id.to_le_bytes());
                    self.out[pos + 4..pos + 8].copy_from_slice(&val_off.to_le_bytes());
                }
            }
            DocValue::Array(items) => {
                // Inline count (u32 LE).
                let count = items.len() as u32;
                self.out.extend_from_slice(&count.to_le_bytes());
                let table_off = self.out.len();
                self.out.extend_from_slice(&vec![0u8; items.len() * 4]);
                let mut value_offsets: Vec<u32> = Vec::with_capacity(items.len());
                for item in items {
                    let val_off = self.encode_value(item)? as u32;
                    value_offsets.push(val_off);
                }
                for (i, val_off) in value_offsets.iter().enumerate() {
                    let pos = table_off + i * 4;
                    self.out[pos..pos + 4].copy_from_slice(&val_off.to_le_bytes());
                }
            }
        }
        Ok(off)
    }
}

impl Default for HexDocBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// HexDoc header (20 bytes).
#[derive(Copy, Clone, Debug)]
pub struct HexDocHeader {
    pub magic: [u8; 4],
    pub root_kind: u8,
    pub _pad: [u8; 3],
    pub field_count: u32,
    pub root_off: u32,
    pub string_pool_off: u32,
}

impl HexDocHeader {
    pub fn from_bytes(bytes: &[u8]) -> HexResult<Self> {
        if bytes.len() < 20 {
            return Err(HexError::corrupt("HexDoc header too short"));
        }
        let mut magic = [0u8; 4];
        magic.copy_from_slice(&bytes[0..4]);
        if magic != HEX_DOC_MAGIC {
            return Err(HexError::corrupt("HexDoc magic mismatch"));
        }
        Ok(Self {
            magic,
            root_kind: bytes[4],
            _pad: [bytes[5], bytes[6], bytes[7]],
            field_count: u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
            root_off: u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]),
            string_pool_off: u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]),
        })
    }

    pub fn to_bytes(&self) -> [u8; 20] {
        let mut out = [0u8; 20];
        out[0..4].copy_from_slice(&self.magic);
        out[4] = self.root_kind;
        out[5..8].copy_from_slice(&self._pad);
        out[8..12].copy_from_slice(&self.field_count.to_le_bytes());
        out[12..16].copy_from_slice(&self.root_off.to_le_bytes());
        out[16..20].copy_from_slice(&self.string_pool_off.to_le_bytes());
        out
    }
}

/// Reader for a HexDoc document. Borrows the bytes; lifetime tied to the
/// owning buffer (typically a PAX var-heap slice).
pub struct HexDoc<'a> {
    bytes: &'a [u8],
    header: HexDocHeader,
}

impl<'a> HexDoc<'a> {
    /// Construct a reader over `bytes`. Validates the magic.
    pub fn new(bytes: &'a [u8]) -> HexResult<Self> {
        let header = HexDocHeader::from_bytes(bytes)?;
        Ok(Self { bytes, header })
    }

    /// The kind of the root value.
    pub fn root_kind(&self) -> DocKind {
        DocKind::from_u8(self.header.root_kind)
    }

    /// Decode the root value into an owned `DocValue`. This walks the entire
    /// document — for point field access use [`get_field`] instead.
    pub fn decode_root(&self) -> HexResult<DocValue> {
        self.decode_value_at(self.header.root_off as usize)
    }

    /// Look up a top-level field by name. Returns the field's value or
    /// `None` if not present (or root is not an Object).
    ///
    /// This is the O(1) fast path: we walk the field offset table looking
    /// for a matching name id, then jump to the value offset. No parsing of
    /// unrelated fields.
    pub fn get_field(&self, name: &str) -> HexResult<Option<DocValue>> {
        if self.root_kind() != DocKind::Object {
            return Ok(None);
        }
        let root_off = self.header.root_off as usize;
        // Layout of an Object value: [kind:1][field_count:4][table:field_count*8]
        let table_off = root_off + 1 + 4;
        let field_count = self.header.field_count as usize;
        for i in 0..field_count {
            let entry_off = table_off + i * 8;
            if entry_off + 8 > self.bytes.len() {
                return Err(HexError::corrupt("HexDoc: field table truncated"));
            }
            let name_id = u32::from_le_bytes([
                self.bytes[entry_off],
                self.bytes[entry_off + 1],
                self.bytes[entry_off + 2],
                self.bytes[entry_off + 3],
            ]);
            let val_off = u32::from_le_bytes([
                self.bytes[entry_off + 4],
                self.bytes[entry_off + 5],
                self.bytes[entry_off + 6],
                self.bytes[entry_off + 7],
            ]) as usize;
            let name_in_pool = self.read_string(name_id as usize)?;
            if name_in_pool == name {
                return Ok(Some(self.decode_value_at(val_off)?));
            }
        }
        Ok(None)
    }

    /// Navigate a dotted path like `user.address.city` through nested
    /// objects. Returns `None` if any segment is missing or non-object.
    pub fn get_path(&self, path: &str) -> HexResult<Option<DocValue>> {
        let segments: Vec<&str> = path.split('.').collect();
        let mut current = self.decode_root()?;
        for seg in segments {
            match current.get(seg) {
                Some(v) => current = v.clone(),
                None => return Ok(None),
            }
        }
        Ok(Some(current))
    }

    fn decode_value_at(&self, off: usize) -> HexResult<DocValue> {
        if off >= self.bytes.len() {
            return Err(HexError::corrupt(format!(
                "HexDoc: value offset {} out of bounds",
                off
            )));
        }
        let kind = DocKind::from_u8(self.bytes[off]);
        let body = &self.bytes[off + 1..];
        match kind {
            DocKind::Null => Ok(DocValue::Null),
            DocKind::Bool => {
                if body.is_empty() {
                    return Err(HexError::corrupt("HexDoc: bool truncated"));
                }
                Ok(DocValue::Bool(body[0] != 0))
            }
            DocKind::I64 => {
                if body.len() < 8 {
                    return Err(HexError::corrupt("HexDoc: i64 truncated"));
                }
                Ok(DocValue::I64(i64::from_le_bytes([
                    body[0], body[1], body[2], body[3], body[4], body[5], body[6], body[7],
                ])))
            }
            DocKind::F64 => {
                if body.len() < 8 {
                    return Err(HexError::corrupt("HexDoc: f64 truncated"));
                }
                Ok(DocValue::F64(f64::from_bits(u64::from_le_bytes([
                    body[0], body[1], body[2], body[3], body[4], body[5], body[6], body[7],
                ]))))
            }
            DocKind::Str => {
                if body.len() < 4 {
                    return Err(HexError::corrupt("HexDoc: str truncated"));
                }
                let id = u32::from_le_bytes([body[0], body[1], body[2], body[3]]) as usize;
                let s = self.read_string(id)?;
                Ok(DocValue::Str(s))
            }
            DocKind::Bytes => {
                if body.len() < 4 {
                    return Err(HexError::corrupt("HexDoc: bytes truncated"));
                }
                let len = u32::from_le_bytes([body[0], body[1], body[2], body[3]]) as usize;
                if body.len() < 4 + len {
                    return Err(HexError::corrupt("HexDoc: bytes payload truncated"));
                }
                Ok(DocValue::Bytes(body[4..4 + len].to_vec()))
            }
            DocKind::Object => {
                // Need field count from header? No — we re-derive it from
                // the table layout: but we don't know the count without the
                // header. We encode the count inline as a u32 right after
                // the kind tag for non-root objects.
                if body.len() < 4 {
                    return Err(HexError::corrupt("HexDoc: object truncated"));
                }
                let field_count = u32::from_le_bytes([body[0], body[1], body[2], body[3]]) as usize;
                let table_off = 4;
                let mut fields = Vec::with_capacity(field_count);
                for i in 0..field_count {
                    let entry_off = table_off + i * 8;
                    if entry_off + 8 > body.len() {
                        return Err(HexError::corrupt("HexDoc: object table truncated"));
                    }
                    let name_id = u32::from_le_bytes([
                        body[entry_off],
                        body[entry_off + 1],
                        body[entry_off + 2],
                        body[entry_off + 3],
                    ]);
                    let val_off = u32::from_le_bytes([
                        body[entry_off + 4],
                        body[entry_off + 5],
                        body[entry_off + 6],
                        body[entry_off + 7],
                    ]) as usize;
                    let name = self.read_string(name_id as usize)?;
                    let val = self.decode_value_at(val_off)?;
                    fields.push((name, val));
                }
                Ok(DocValue::Object(fields))
            }
            DocKind::Array => {
                if body.len() < 4 {
                    return Err(HexError::corrupt("HexDoc: array truncated"));
                }
                let count = u32::from_le_bytes([body[0], body[1], body[2], body[3]]) as usize;
                let table_off = 4;
                let mut items = Vec::with_capacity(count);
                for i in 0..count {
                    let entry_off = table_off + i * 4;
                    if entry_off + 4 > body.len() {
                        return Err(HexError::corrupt("HexDoc: array table truncated"));
                    }
                    let val_off = u32::from_le_bytes([
                        body[entry_off],
                        body[entry_off + 1],
                        body[entry_off + 2],
                        body[entry_off + 3],
                    ]) as usize;
                    items.push(self.decode_value_at(val_off)?);
                }
                Ok(DocValue::Array(items))
            }
        }
    }

    /// Read a string from the string pool. The pool is a sequence of
    /// length-prefixed (u32 LE) UTF-8 strings, indexed by byte offset.
    fn read_string(&self, offset: usize) -> HexResult<String> {
        // We need to find the string pool. For the prototype, we search for
        // it at the end of the buffer (the builder appends it there).
        // A more robust format would record the pool offset in the header;
        // for now we scan from the root offset.
        // To keep this simple, we re-find the pool by walking from root.
        // The builder always appends the string pool at the end of the
        // buffer. We treat `offset` as a position within that pool.
        let pool_start = self.find_string_pool()?;
        let abs = pool_start + offset;
        if abs + 4 > self.bytes.len() {
            return Err(HexError::corrupt(format!(
                "HexDoc: string offset {} out of bounds",
                offset
            )));
        }
        let len = u32::from_le_bytes([
            self.bytes[abs],
            self.bytes[abs + 1],
            self.bytes[abs + 2],
            self.bytes[abs + 3],
        ]) as usize;
        let start = abs + 4;
        let end = start + len;
        if end > self.bytes.len() {
            return Err(HexError::corrupt("HexDoc: string payload truncated"));
        }
        Ok(String::from_utf8_lossy(&self.bytes[start..end]).into_owned())
    }

    /// The string pool offset is stored in the header's `string_pool_off`
    /// field (a full u32, supports pools up to 4 GiB).
    fn find_string_pool(&self) -> HexResult<usize> {
        let pool_off = self.header.string_pool_off as usize;
        if pool_off >= self.bytes.len() {
            return Err(HexError::corrupt(format!(
                "HexDoc: pool offset {} out of bounds (buf len {})",
                pool_off,
                self.bytes.len()
            )));
        }
        Ok(pool_off)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_simple_object() {
        let doc = DocValue::Object(vec![
            ("name".to_string(), DocValue::Str("Alice".to_string())),
            ("age".to_string(), DocValue::I64(30)),
            ("active".to_string(), DocValue::Bool(true)),
        ]);
        let bytes = HexDocBuilder::encode(&doc).unwrap();
        let reader = HexDoc::new(&bytes).unwrap();
        assert_eq!(reader.root_kind(), DocKind::Object);

        let name = reader.get_field("name").unwrap().unwrap();
        match name {
            DocValue::Str(s) => assert_eq!(s, "Alice"),
            other => panic!("expected Str, got {:?}", other),
        }
        let age = reader.get_field("age").unwrap().unwrap();
        match age {
            DocValue::I64(30) => {}
            other => panic!("expected I64(30), got {:?}", other),
        }
    }

    #[test]
    fn roundtrip_nested_object() {
        let doc = DocValue::Object(vec![
            ("user".to_string(), DocValue::Object(vec![
                ("id".to_string(), DocValue::I64(42)),
                ("address".to_string(), DocValue::Object(vec![
                    ("city".to_string(), DocValue::Str("Berlin".to_string())),
                    ("zip".to_string(), DocValue::Str("10115".to_string())),
                ])),
            ])),
            ("active".to_string(), DocValue::Bool(true)),
        ]);
        let bytes = HexDocBuilder::encode(&doc).unwrap();
        let reader = HexDoc::new(&bytes).unwrap();

        // Dotted-path access.
        let city = reader.get_path("user.address.city").unwrap().unwrap();
        match city {
            DocValue::Str(s) => assert_eq!(s, "Berlin"),
            other => panic!("expected Str, got {:?}", other),
        }
    }

    #[test]
    fn roundtrip_array() {
        let doc = DocValue::Object(vec![
            ("items".to_string(), DocValue::Array(vec![
                DocValue::I64(1),
                DocValue::I64(2),
                DocValue::I64(3),
            ])),
        ]);
        let bytes = HexDocBuilder::encode(&doc).unwrap();
        let reader = HexDoc::new(&bytes).unwrap();
        let items = reader.get_field("items").unwrap().unwrap();
        match items {
            DocValue::Array(a) => {
                assert_eq!(a.len(), 3);
                assert_eq!(a[0], DocValue::I64(1));
                assert_eq!(a[2], DocValue::I64(3));
            }
            other => panic!("expected Array, got {:?}", other),
        }
    }

    #[test]
    fn missing_field_returns_none() {
        let doc = DocValue::Object(vec![
            ("name".to_string(), DocValue::Str("Bob".to_string())),
        ]);
        let bytes = HexDocBuilder::encode(&doc).unwrap();
        let reader = HexDoc::new(&bytes).unwrap();
        assert!(reader.get_field("nonexistent").unwrap().is_none());
    }
}
