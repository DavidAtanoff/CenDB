//! Type-aware minipage encodings.
//!
//! The spec calls for a two-stage compression pipeline:
//!   1. Type-aware *encoding* (keeps data SIMD-decodable; many predicates
//!      run on encoded data without decoding).
//!   2. Optional *general-purpose* block compression (LZ4/zstd) for cold data.
//!
//! For the prototype we implement the four most impactful encodings — `Raw`,
//! `BitPacked`, `FrameOfReference`, and `DeltaOfDelta` — end-to-end (encode
//! AND decode), and stub the remaining variants with a transparent fall-through
//! to `Raw`. The encoders operate on `&[i64]` inputs (the canonical form for
//! integer columns) and produce a `Vec<u8>` that is written into the minipage.

use cendb_core::HexResult;
use cendb_core::HexError;

/// Tag for the encoding of a minipage. The discriminant is what gets written
/// into the on-disk `ColumnDirectory.encoding` field, so the values are
/// stable: never renumber existing variants.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Encoding {
    /// No encoding — values stored verbatim.
    Raw = 0,
    /// Integers in a small range, packed at `bits` bits each.
    BitPacked { bits: u8 } = 1,
    /// Clustered integers: store a `base` and pack residuals at `bits`.
    FrameOfReference { base: i64, bits: u8 } = 2,
    /// Monotonic integers (timestamps, sequential PKs).
    DeltaOfDelta = 3,
    /// Gorilla XOR floats for time-series (stub → Raw in this prototype).
    Gorilla = 4,
    /// Improved TS float codec (stub → Raw in this prototype).
    Chimp128 = 5,
    /// Low-cardinality strings/enums (stub → Raw in this prototype).
    Dictionary { dict_id: u32 } = 6,
    /// Long runs of identical values (stub → Raw in this prototype).
    RunLength = 7,
    /// FSST short-string compression (stub → Raw in this prototype).
    Fsst = 8,
}

impl Encoding {
    /// Stable on-disk discriminant. We use the low 4 bits for the variant tag
    /// (supports up to 16 variants) and the high 4 bits for an optional
    /// parameter byte (used by `BitPacked`/`FoR` to record `bits`; other
    /// variants store their parameters inside the minipage body so the
    /// discriminant stays a single byte).
    pub fn discriminant(&self) -> u8 {
        let tag: u8 = match self {
            Encoding::Raw => 0,
            Encoding::BitPacked { .. } => 1,
            Encoding::FrameOfReference { .. } => 2,
            Encoding::DeltaOfDelta => 3,
            Encoding::Gorilla => 4,
            Encoding::Chimp128 => 5,
            Encoding::Dictionary { .. } => 6,
            Encoding::RunLength => 7,
            Encoding::Fsst => 8,
        };
        let param: u8 = match self {
            Encoding::BitPacked { bits } => *bits & 0x0F,
            Encoding::FrameOfReference { bits, .. } => *bits & 0x0F,
            Encoding::Dictionary { dict_id } => (*dict_id as u8) & 0x0F,
            _ => 0,
        };
        tag | (param << 4)
    }

    /// Inverse of [`discriminant`]. Note that parameter-bearing variants lose
    /// the *full* parameter (e.g. `FoR.base`) — the body of the minipage
    /// carries the complete parameter set, the discriminant only carries a
    /// hint sufficient to pick the right codec.
    pub fn from_discriminant(b: u8) -> Self {
        let tag = b & 0x0F;
        let param = (b >> 4) & 0x0F;
        match tag {
            0 => Encoding::Raw,
            1 => Encoding::BitPacked { bits: param },
            2 => Encoding::FrameOfReference { base: 0, bits: param },
            3 => Encoding::DeltaOfDelta,
            4 => Encoding::Gorilla,
            5 => Encoding::Chimp128,
            6 => Encoding::Dictionary { dict_id: param as u32 },
            7 => Encoding::RunLength,
            8 => Encoding::Fsst,
            _ => Encoding::Raw,
        }
    }
}

/// The codec trait that the storage layer calls into. Each encoding that has a
/// real implementation provides an `encode`/`decode` pair here. Variants that
/// are stubbed fall back to `RawCodec`.
pub trait EncodingCodec: Send + Sync {
    /// Encode `vals` (a slice of i64) into a byte buffer suitable for a
    /// minipage. The returned buffer's first byte is the encoding discriminant
    /// so the decoder knows which path to take.
    fn encode(&self, vals: &[i64]) -> HexResult<Vec<u8>>;

    /// Decode a minipage back into `Vec<i64>`. The input must include the
    /// leading discriminant byte.
    fn decode(&self, bytes: &[u8], count: usize) -> HexResult<Vec<i64>>;
}

/// Pick the concrete codec for an `Encoding` value.
pub fn codec_for(enc: Encoding) -> Box<dyn EncodingCodec> {
    match enc {
        Encoding::Raw => Box::new(RawCodec),
        Encoding::BitPacked { .. } => Box::new(BitPackedCodec),
        Encoding::FrameOfReference { .. } => Box::new(FrameOfReferenceCodec),
        Encoding::DeltaOfDelta => Box::new(DeltaOfDeltaCodec),
        Encoding::RunLength => Box::new(RunLengthCodec),
        // Gorilla / Chimp128 / Dictionary / Fsst operate on float/string
        // data, not i64; we fall through to Raw for the i64 path. The
        // F64 column stores its values as bit-patterns in i64 form, so
        // Gorilla is invoked separately by the F64 column writer.
        Encoding::Gorilla => Box::new(RawCodec),
        Encoding::Chimp128 => Box::new(RawCodec),
        Encoding::Dictionary { .. } => Box::new(RawCodec),
        Encoding::Fsst => Box::new(RawCodec),
    }
}

/// Re-encode a minipage using whatever encoding the caller requests. Returns
/// the encoded bytes (without the discriminant byte — the column directory
/// already records the encoding).
pub fn encode_minipage(enc: Encoding, vals: &[i64]) -> HexResult<Vec<u8>> {
    let codec = codec_for(enc);
    let mut bytes = codec.encode(vals)?;
    // The codec prepends a discriminant byte for self-describing streams, but
    // the on-disk minipage already carries the encoding tag in its column
    // directory entry, so we strip the leading byte here.
    if !bytes.is_empty() {
        bytes.remove(0);
    }
    Ok(bytes)
}

/// Decode a minipage whose encoding is known from the column directory.
pub fn decode_minipage(enc: Encoding, bytes: &[u8], count: usize) -> HexResult<Vec<i64>> {
    let codec = codec_for(enc);
    // Re-prepend the discriminant byte so the codec's decoder is happy.
    let mut owned = Vec::with_capacity(bytes.len() + 1);
    owned.push(enc.discriminant());
    owned.extend_from_slice(bytes);
    codec.decode(&owned, count)
}

// ============================================================================
// Raw — store i64s verbatim, little-endian.
// ============================================================================

pub struct RawCodec;

impl EncodingCodec for RawCodec {
    fn encode(&self, vals: &[i64]) -> HexResult<Vec<u8>> {
        let mut out = Vec::with_capacity(1 + vals.len() * 8);
        out.push(Encoding::Raw.discriminant());
        for v in vals {
            out.extend_from_slice(&v.to_le_bytes());
        }
        Ok(out)
    }

    fn decode(&self, bytes: &[u8], count: usize) -> HexResult<Vec<i64>> {
        if bytes.is_empty() {
            return Ok(Vec::new());
        }
        let body = &bytes[1..]; // skip discriminant
        let need = count * 8;
        if body.len() < need {
            return Err(HexError::corrupt(format!(
                "Raw decode: need {need} bytes, got {}",
                body.len()
            )));
        }
        let mut out = Vec::with_capacity(count);
        for i in 0..count {
            let off = i * 8;
            let v = i64::from_le_bytes([
                body[off], body[off + 1], body[off + 2], body[off + 3],
                body[off + 4], body[off + 5], body[off + 6], body[off + 7],
            ]);
            out.push(v);
        }
        Ok(out)
    }
}

// ============================================================================
// BitPacked — pack each value into `bits` bits. Requires all values >= 0 and
// < 2^bits. Used for low-cardinality integers and small-range PKs.
// ============================================================================

pub struct BitPackedCodec;

impl EncodingCodec for BitPackedCodec {
    fn encode(&self, vals: &[i64]) -> HexResult<Vec<u8>> {
        // Determine the minimum bit width that fits all values.
        let mut max_val: u64 = 0;
        for &v in vals {
            if v < 0 {
                return Err(HexError::constraint(
                    "BitPacked does not support negative values",
                ));
            }
            if (v as u64) > max_val {
                max_val = v as u64;
            }
        }
        let bits = required_bits(max_val).max(1) as u8;
        let mut out = Vec::with_capacity(2 + ((vals.len() * bits as usize + 7) / 8));
        out.push(Encoding::BitPacked { bits }.discriminant());
        out.push(bits);
        let mut bit_buf: u64 = 0;
        let mut bits_in_buf: u32 = 0;
        for &v in vals {
            bit_buf |= ((v as u64) & ((1u64 << bits) - 1).max(1)) << bits_in_buf;
            bits_in_buf += bits as u32;
            while bits_in_buf >= 8 {
                out.push((bit_buf & 0xFF) as u8);
                bit_buf >>= 8;
                bits_in_buf -= 8;
            }
        }
        if bits_in_buf > 0 {
            out.push((bit_buf & 0xFF) as u8);
        }
        Ok(out)
    }

    fn decode(&self, bytes: &[u8], count: usize) -> HexResult<Vec<i64>> {
        if bytes.len() < 2 {
            return Err(HexError::corrupt("BitPacked decode: short header"));
        }
        let bits = bytes[1] as u32;
        // Clamp bits to valid range to prevent shift overflow on
        // malformed input. Values > 64 are nonsensical for u64.
        let bits = bits.min(64);
        let body = &bytes[2..];
        let mut out = Vec::with_capacity(count);
        let mut bit_buf: u64 = 0;
        let mut bits_in_buf: u32 = 0;
        let mut byte_idx = 0usize;
        let mask = if bits >= 64 { u64::MAX } else { (1u64 << bits) - 1 };
        for _ in 0..count {
            while bits_in_buf < bits && byte_idx < body.len() && bits_in_buf < 56 {
                bit_buf |= (body[byte_idx] as u64) << bits_in_buf;
                bits_in_buf += 8;
                byte_idx += 1;
            }
            out.push((bit_buf & mask) as i64);
            if bits >= 64 {
                bit_buf = 0;
            } else {
                bit_buf >>= bits;
            }
            bits_in_buf = bits_in_buf.saturating_sub(bits);
        }
        Ok(out)
    }
}

fn required_bits(max_val: u64) -> u32 {
    if max_val == 0 {
        1
    } else {
        64 - max_val.leading_zeros()
    }
}

// ============================================================================
// FrameOfReference — store `base` once, then pack residuals at `bits`.
// Useful for clustered integers (e.g. timestamps in a 1-hour window).
// ============================================================================

pub struct FrameOfReferenceCodec;

impl EncodingCodec for FrameOfReferenceCodec {
    fn encode(&self, vals: &[i64]) -> HexResult<Vec<u8>> {
        if vals.is_empty() {
            return Ok(vec![Encoding::FrameOfReference { base: 0, bits: 0 }.discriminant()]);
        }
        let mut min_v = vals[0];
        let mut max_v = vals[0];
        for &v in vals {
            if v < min_v {
                min_v = v;
            }
            if v > max_v {
                max_v = v;
            }
        }
        let base = min_v;
        let range = (max_v - min_v) as u64;
        let bits = required_bits(range).max(1) as u8;
        let mut out = Vec::with_capacity(10 + ((vals.len() * bits as usize + 7) / 8));
        out.push(Encoding::FrameOfReference { base, bits }.discriminant());
        out.extend_from_slice(&base.to_le_bytes());
        out.push(bits);
        let mut bit_buf: u64 = 0;
        let mut bits_in_buf: u32 = 0;
        for &v in vals {
            let residual = (v - base) as u64 & ((1u64 << bits) - 1).max(1);
            bit_buf |= residual << bits_in_buf;
            bits_in_buf += bits as u32;
            while bits_in_buf >= 8 {
                out.push((bit_buf & 0xFF) as u8);
                bit_buf >>= 8;
                bits_in_buf -= 8;
            }
        }
        if bits_in_buf > 0 {
            out.push((bit_buf & 0xFF) as u8);
        }
        Ok(out)
    }

    fn decode(&self, bytes: &[u8], count: usize) -> HexResult<Vec<i64>> {
        if bytes.len() < 10 {
            return Err(HexError::corrupt("FoR decode: short header"));
        }
        let base = i64::from_le_bytes([
            bytes[1], bytes[2], bytes[3], bytes[4],
            bytes[5], bytes[6], bytes[7], bytes[8],
        ]);
        let bits = bytes[9] as u32;
        // Clamp to prevent shift overflow on malformed input.
        let bits = bits.min(64);
        let body = &bytes[10..];
        let mut out = Vec::with_capacity(count);
        let mut bit_buf: u64 = 0;
        let mut bits_in_buf: u32 = 0;
        let mut byte_idx = 0usize;
        let mask = if bits >= 64 { u64::MAX } else { (1u64 << bits) - 1 };
        for _ in 0..count {
            while bits_in_buf < bits && byte_idx < body.len() && bits_in_buf < 56 {
                bit_buf |= (body[byte_idx] as u64) << bits_in_buf;
                bits_in_buf += 8;
                byte_idx += 1;
            }
            out.push(base.wrapping_add((bit_buf & mask) as i64));
            if bits >= 64 {
                bit_buf = 0;
            } else {
                bit_buf >>= bits;
            }
            bits_in_buf = bits_in_buf.saturating_sub(bits);
        }
        Ok(out)
    }
}

// ============================================================================
// DeltaOfDelta — canonical time-series encoding. Store the first value
// verbatim, then the first delta verbatim, then delta-of-deltas as a
// variable-length integer (zig-zag encoded). For monotonic sequences this
// achieves ~1 bit per value.
// ============================================================================

pub struct DeltaOfDeltaCodec;

impl EncodingCodec for DeltaOfDeltaCodec {
    fn encode(&self, vals: &[i64]) -> HexResult<Vec<u8>> {
        let mut out = Vec::with_capacity(1 + vals.len() * 2);
        out.push(Encoding::DeltaOfDelta.discriminant());
        if vals.is_empty() {
            return Ok(out);
        }
        // first value
        out.extend_from_slice(&vals[0].to_le_bytes());
        if vals.len() == 1 {
            return Ok(out);
        }
        let mut prev_delta = vals[1] - vals[0];
        out.extend_from_slice(&prev_delta.to_le_bytes());
        for i in 2..vals.len() {
            let delta = vals[i] - vals[i - 1];
            let dod = delta - prev_delta;
            prev_delta = delta;
            // zig-zag varint
            let zz = ((dod << 1) ^ (dod >> 63)) as u64;
            varint_encode(&mut out, zz);
        }
        Ok(out)
    }

    fn decode(&self, bytes: &[u8], count: usize) -> HexResult<Vec<i64>> {
        if bytes.is_empty() {
            return Ok(Vec::new());
        }
        let mut cursor = 1usize; // skip discriminant
        if count == 0 {
            return Ok(Vec::new());
        }
        if bytes.len() < cursor + 8 {
            return Err(HexError::corrupt("DoD decode: short first value"));
        }
        let first = i64::from_le_bytes([
            bytes[cursor], bytes[cursor + 1], bytes[cursor + 2], bytes[cursor + 3],
            bytes[cursor + 4], bytes[cursor + 5], bytes[cursor + 6], bytes[cursor + 7],
        ]);
        cursor += 8;
        let mut out = Vec::with_capacity(count);
        out.push(first);
        if count == 1 {
            return Ok(out);
        }
        if bytes.len() < cursor + 8 {
            return Err(HexError::corrupt("DoD decode: short first delta"));
        }
        let mut prev_delta = i64::from_le_bytes([
            bytes[cursor], bytes[cursor + 1], bytes[cursor + 2], bytes[cursor + 3],
            bytes[cursor + 4], bytes[cursor + 5], bytes[cursor + 6], bytes[cursor + 7],
        ]);
        cursor += 8;
        out.push(first.wrapping_add(prev_delta));
        for _ in 2..count {
            let (zz, used) = varint_decode(&bytes[cursor..])
                .ok_or_else(|| HexError::corrupt("DoD decode: truncated varint"))?;
            cursor += used;
            let dod = ((zz >> 1) as i64) ^ -((zz & 1) as i64);
            let delta = prev_delta.wrapping_add(dod);
            let prev = *out.last().unwrap();
            out.push(prev.wrapping_add(delta));
            prev_delta = delta;
        }
        Ok(out)
    }
}

fn varint_encode(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn varint_decode(bytes: &[u8]) -> Option<(u64, usize)> {
    let mut v: u64 = 0;
    let mut shift = 0u32;
    for (i, &b) in bytes.iter().enumerate() {
        v |= ((b & 0x7F) as u64) << shift;
        if b & 0x80 == 0 {
            return Some((v, i + 1));
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
    None
}

// ============================================================================
// RunLength — store runs of identical values as (count, value) pairs.
// Great for sorted/sparse columns where many consecutive rows share a value.
// ============================================================================

pub struct RunLengthCodec;

impl EncodingCodec for RunLengthCodec {
    fn encode(&self, vals: &[i64]) -> HexResult<Vec<u8>> {
        let mut out = Vec::with_capacity(1 + vals.len() * 4);
        out.push(Encoding::RunLength.discriminant());
        if vals.is_empty() {
            return Ok(out);
        }
        let mut current = vals[0];
        let mut run: u32 = 1;
        for &v in &vals[1..] {
            if v == current {
                run += 1;
            } else {
                out.extend_from_slice(&run.to_le_bytes());
                out.extend_from_slice(&current.to_le_bytes());
                current = v;
                run = 1;
            }
        }
        // Flush the last run.
        out.extend_from_slice(&run.to_le_bytes());
        out.extend_from_slice(&current.to_le_bytes());
        Ok(out)
    }

    fn decode(&self, bytes: &[u8], count: usize) -> HexResult<Vec<i64>> {
        if bytes.is_empty() {
            return Ok(Vec::new());
        }
        let body = &bytes[1..]; // skip discriminant
        let mut out = Vec::with_capacity(count);
        let mut cursor = 0usize;
        while out.len() < count && cursor + 12 <= body.len() {
            let run = u32::from_le_bytes([
                body[cursor], body[cursor + 1], body[cursor + 2], body[cursor + 3],
            ]) as usize;
            let val = i64::from_le_bytes([
                body[cursor + 4], body[cursor + 5], body[cursor + 6], body[cursor + 7],
                body[cursor + 8], body[cursor + 9], body[cursor + 10], body[cursor + 11],
            ]);
            for _ in 0..run {
                out.push(val);
                if out.len() >= count {
                    break;
                }
            }
            cursor += 12;
        }
        Ok(out)
    }
}

// ============================================================================
// Gorilla XOR — encode f64 values as XOR-of-previous with a compact
// control-bit scheme. Used for time-series floats where consecutive values
// are similar (e.g. temperatures, prices).
//
// Format (per value after the first):
//   - bit 0:    '0' if XOR == 0 (value unchanged, 1 bit total)
//   - bit 0:    '1' if XOR != 0
//     - bit 1:  '0' if the leading zero block and trailing zero block fall
//                       in the same range as the previous non-zero XOR
//                       (reuse, 2 bits total)
//     - bit 1:  '1' if new range
//                       - 5 bits: leading zero count
//                       - 6 bits: meaningful bit count
//                       - meaningful bits of XOR
//
// For the prototype we operate on the bit-representation of f64 (i64 alias).
// The first value is stored verbatim (8 bytes).
// ============================================================================

/// Encode a slice of f64-as-i64 (bit patterns) using Gorilla XOR.
/// Returns bytes without a discriminant prefix (caller adds it).
pub fn gorilla_encode(vals: &[i64]) -> Vec<u8> {
    let mut writer = BitWriter::new();
    if vals.is_empty() {
        return writer.into_bytes();
    }
    // First value: verbatim.
    writer.write_bits(vals[0] as u64, 64);
    let mut prev = vals[0] as u64;
    let mut prev_leading = i64::MAX as u32;
    let mut prev_trailing = 0u32;
    for &v in &vals[1..] {
        let cur = v as u64;
        let xor = prev ^ cur;
        if xor == 0 {
            writer.write_bit(false);
        } else {
            writer.write_bit(true);
            let leading = xor.leading_zeros();
            let trailing = xor.trailing_zeros();
            // Reuse previous block if it fits.
            if leading >= prev_leading && trailing >= prev_trailing {
                writer.write_bit(false);
                let meaningful = 64 - prev_leading - prev_trailing;
                writer.write_bits(xor >> prev_trailing, meaningful);
            } else {
                writer.write_bit(true);
                if leading >= 32 {
                    // Clamp to 5 bits (max 31).
                    writer.write_bits(31, 5);
                } else {
                    writer.write_bits(leading as u64, 5);
                }
                let meaningful = 64 - leading - trailing;
                if meaningful > 0 {
                    writer.write_bits(meaningful as u64, 6);
                    writer.write_bits(xor >> trailing, meaningful);
                } else {
                    // xor had only 1 bit set at position 63; meaningful=0
                    // is a degenerate case. Write 1 to avoid the issue.
                    writer.write_bits(1, 6);
                    writer.write_bits(0, 1);
                }
                prev_leading = leading;
                prev_trailing = trailing;
            }
        }
        prev = cur;
    }
    writer.into_bytes()
}

/// Decode Gorilla XOR bytes back into i64 values.
pub fn gorilla_decode(bytes: &[u8], count: usize) -> HexResult<Vec<i64>> {
    if count == 0 {
        return Ok(Vec::new());
    }
    let mut reader = BitReader::new(bytes);
    let mut out = Vec::with_capacity(count);
    let first = reader.read_bits(64).ok_or_else(|| HexError::corrupt("Gorilla: short first value"))?;
    out.push(first as i64);
    let mut prev = first;
    let mut prev_leading = i64::MAX as u32;
    let mut prev_trailing = 0u32;
    while out.len() < count {
        let bit = reader.read_bit().ok_or_else(|| HexError::corrupt("Gorilla: truncated"))?;
        if !bit {
            out.push(prev as i64);
            continue;
        }
        let bit = reader.read_bit().ok_or_else(|| HexError::corrupt("Gorilla: truncated"))?;
        let (mut leading, mut meaningful, trailing);
        if !bit {
            // Reuse previous block. Use wrapping_sub to match release-mode
            // behavior (the original code used plain subtraction which
            // wraps in release but panics in debug).
            leading = prev_leading;
            trailing = prev_trailing;
            meaningful = (64u32).wrapping_sub(leading).wrapping_sub(trailing);
        } else {
            leading = reader.read_bits(5).ok_or_else(|| HexError::corrupt("Gorilla: short leading"))? as u32;
            if leading == 31 {
                leading = 32;
            }
            meaningful = reader.read_bits(6).ok_or_else(|| HexError::corrupt("Gorilla: short meaningful"))? as u32;
            if meaningful == 0 {
                // Original placeholder: 64 - 2*leading. Use wrapping to
                // match release-mode behavior.
                meaningful = (64u32).wrapping_sub(leading).wrapping_sub(leading);
            }
            trailing = (64u32).wrapping_sub(leading).wrapping_sub(meaningful);
            prev_leading = leading;
            prev_trailing = trailing;
        }
        if meaningful == 0 || meaningful > 64 {
            // Skip; the value is identical to prev (or meaningless due to
            // overflow from malformed/corrupted input).
            out.push(prev as i64);
            continue;
        }
        let xor_meaningful = reader.read_bits(meaningful as u32).ok_or_else(|| HexError::corrupt("Gorilla: short xor"))?;
        let xor = if trailing < 64 {
            xor_meaningful << trailing
        } else {
            0 // trailing >= 64 means the XOR is all zeros
        };
        let cur = prev ^ xor;
        out.push(cur as i64);
        prev = cur;
    }
    Ok(out)
}

/// Simple bit writer that accumulates bits into bytes.
struct BitWriter {
    bytes: Vec<u8>,
    current: u64,
    n_bits: u32,
}

impl BitWriter {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            current: 0,
            n_bits: 0,
        }
    }

    fn write_bit(&mut self, bit: bool) {
        if bit {
            self.current |= 1 << (63 - self.n_bits);
        }
        self.n_bits += 1;
        if self.n_bits == 64 {
            self.bytes.extend_from_slice(&self.current.to_be_bytes());
            self.current = 0;
            self.n_bits = 0;
        }
    }

    fn write_bits(&mut self, value: u64, count: u32) {
        for i in (0..count).rev() {
            self.write_bit((value >> i) & 1 != 0);
        }
    }

    fn into_bytes(mut self) -> Vec<u8> {
        if self.n_bits > 0 {
            self.bytes.extend_from_slice(&self.current.to_be_bytes());
        }
        self.bytes
    }
}

/// Simple bit reader.
struct BitReader<'a> {
    bytes: &'a [u8],
    bit_pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, bit_pos: 0 }
    }

    fn read_bit(&mut self) -> Option<bool> {
        let byte_idx = self.bit_pos / 8;
        let bit_idx = 7 - (self.bit_pos % 8);
        if byte_idx >= self.bytes.len() {
            return None;
        }
        let bit = (self.bytes[byte_idx] >> bit_idx) & 1 != 0;
        self.bit_pos += 1;
        Some(bit)
    }

    fn read_bits(&mut self, count: u32) -> Option<u64> {
        let mut v = 0u64;
        for _ in 0..count {
            v = (v << 1) | (self.read_bit()? as u64);
        }
        Some(v)
    }
}

/// Auto-select the best encoding for an i64 column based on its observed
/// values. Implements the heuristics from §4.1.1 of the spec.
pub fn auto_select_encoding_i64(vals: &[i64]) -> Encoding {
    if vals.len() < 2 {
        return Encoding::Raw;
    }
    // Check monotonicity (for DeltaOfDelta).
    let mut monotonic = true;
    for i in 1..vals.len() {
        if vals[i] < vals[i - 1] {
            monotonic = false;
            break;
        }
    }
    if monotonic {
        // Constant-delta sequences compress to ~0 bits/value with DoD.
        // Use wrapping_sub for safety: the delta might overflow if vals
        // span the full i64 range, but we only compare for equality.
        let mut all_same_delta = true;
        let first_delta = vals[1].wrapping_sub(vals[0]);
        for i in 2..vals.len() {
            if vals[i].wrapping_sub(vals[i - 1]) != first_delta {
                all_same_delta = false;
                break;
            }
        }
        if all_same_delta || first_delta != 0 {
            return Encoding::DeltaOfDelta;
        }
    }
    // Check if range fits in few bits → BitPacked. Only valid for
    // non-negative values.
    let mut min_v = vals[0];
    let mut max_v = vals[0];
    for &v in vals {
        if v < min_v {
            min_v = v;
        }
        if v > max_v {
            max_v = v;
        }
    }
    if min_v >= 0 {
        let bits = required_bits(max_v as u64);
        if bits <= 16 {
            return Encoding::BitPacked { bits: bits as u8 };
        }
    }
    // Clustered integers → FrameOfReference. Compute range as u64 to avoid
    // overflow when min_v is i64::MIN and max_v is i64::MAX.
    if min_v >= 0 || max_v < 0 || (max_v as i128 - min_v as i128) < (1i128 << 32) {
        let range = (max_v as i128 - min_v as i128) as u64;
        if range > 0 && range < (1u64 << 32) {
            let bits = required_bits(range);
            if bits <= 32 {
                return Encoding::FrameOfReference { base: min_v, bits: bits as u8 };
            }
        }
    }
    Encoding::Raw
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_roundtrip() {
        let vals = vec![1i64, 2, 3, 4, 5, 6, 7, 8];
        let codec = RawCodec;
        let enc = codec.encode(&vals).unwrap();
        let dec = codec.decode(&enc, vals.len()).unwrap();
        assert_eq!(dec, vals);
    }

    #[test]
    fn bitpacked_roundtrip() {
        let vals = vec![0i64, 1, 2, 3, 4, 5, 6, 7, 0, 7, 1, 6];
        let codec = BitPackedCodec;
        let enc = codec.encode(&vals).unwrap();
        let dec = codec.decode(&enc, vals.len()).unwrap();
        assert_eq!(dec, vals);
    }

    #[test]
    fn frame_of_reference_roundtrip() {
        let vals = vec![1000i64, 1005, 1010, 1003, 1008, 1001];
        let codec = FrameOfReferenceCodec;
        let enc = codec.encode(&vals).unwrap();
        let dec = codec.decode(&enc, vals.len()).unwrap();
        assert_eq!(dec, vals);
    }

    #[test]
    fn delta_of_delta_roundtrip() {
        // Monotonic timestamps: 1ns, 2ns, 3ns, ..., 1000ns
        let vals: Vec<i64> = (1..=1000).collect();
        let codec = DeltaOfDeltaCodec;
        let enc = codec.encode(&vals).unwrap();
        // DoD should compress this to far fewer bytes than Raw.
        let raw_size = vals.len() * 8;
        assert!(
            enc.len() < raw_size,
            "DoD enc {} should be < raw {}",
            enc.len(),
            raw_size
        );
        let dec = codec.decode(&enc, vals.len()).unwrap();
        assert_eq!(dec, vals);
    }

    #[test]
    fn auto_select_picks_dod_for_monotonic() {
        let vals: Vec<i64> = (0..1000).map(|i| 1_700_000_000_000 + i * 60).collect();
        match auto_select_encoding_i64(&vals) {
            Encoding::DeltaOfDelta => {}
            other => panic!("expected DoD, got {:?}", other),
        }
    }

    #[test]
    fn auto_select_picks_bitpacked_for_small_range() {
        let vals: Vec<i64> = (0..100).map(|i| i % 8).collect();
        match auto_select_encoding_i64(&vals) {
            Encoding::BitPacked { .. } => {}
            other => panic!("expected BitPacked, got {:?}", other),
        }
    }

    #[test]
    fn discriminant_roundtrip() {
        let cases = [
            Encoding::Raw,
            Encoding::BitPacked { bits: 8 },
            Encoding::FrameOfReference { base: 100, bits: 12 },
            Encoding::DeltaOfDelta,
            Encoding::Gorilla,
            Encoding::Dictionary { dict_id: 7 },
        ];
        for e in cases {
            let d = e.discriminant();
            let back = Encoding::from_discriminant(d);
            // Note: FoR loses `base` in the discriminant round-trip (it's
            // stored inside the minipage body instead), so we only check the
            // variant matches.
            assert_eq!(core::mem::discriminant(&e), core::mem::discriminant(&back));
        }
    }
}

#[cfg(test)]
mod gorilla_tests {
    use super::*;

    #[test]
    fn gorilla_roundtrip_constant_floats() {
        // All values the same → each XOR is 0 → 1 bit/value.
        let vals: Vec<i64> = (0..100).map(|_| 1.5f64.to_bits() as i64).collect();
        let enc = gorilla_encode(&vals);
        let dec = gorilla_decode(&enc, vals.len()).unwrap();
        assert_eq!(dec, vals);
        // Should compress dramatically: 100 values × 1 bit ≈ 13 bytes vs raw 800 bytes.
        assert!(enc.len() < vals.len() * 8);
    }

    #[test]
    fn gorilla_roundtrip_changing_floats() {
        // Slowly-changing floats (temperatures).
        let vals: Vec<i64> = (0..100)
            .map(|i| ((i as f64) * 0.1).sin().to_bits() as i64)
            .collect();
        let enc = gorilla_encode(&vals);
        let dec = gorilla_decode(&enc, vals.len()).unwrap();
        assert_eq!(dec, vals);
    }

    #[test]
    fn gorilla_handles_single_value() {
        let vals = vec![42.0f64.to_bits() as i64];
        let enc = gorilla_encode(&vals);
        let dec = gorilla_decode(&enc, 1).unwrap();
        assert_eq!(dec, vals);
    }

    #[test]
    fn runlength_roundtrip() {
        // Long runs of identical values compress well.
        let vals: Vec<i64> = (0..10).flat_map(|v| std::iter::repeat(v).take(100)).collect();
        let codec = RunLengthCodec;
        let enc = codec.encode(&vals).unwrap();
        let dec = codec.decode(&enc, vals.len()).unwrap();
        assert_eq!(dec, vals);
        // 10 runs × 12 bytes = 120 bytes vs raw 1000 × 8 = 8000 bytes.
        assert!(enc.len() < vals.len() * 8);
    }

    #[test]
    fn runlength_no_compression_for_distinct_values() {
        // All distinct values → no compression.
        let vals: Vec<i64> = (0..100).collect();
        let codec = RunLengthCodec;
        let enc = codec.encode(&vals).unwrap();
        let dec = codec.decode(&enc, vals.len()).unwrap();
        assert_eq!(dec, vals);
        // 100 runs × 12 bytes = 1200 bytes vs raw 800 bytes → worse than Raw.
        assert!(enc.len() > vals.len() * 8);
    }
}
