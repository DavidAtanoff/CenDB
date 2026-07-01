//! Safe-by-construction zero-copy casting helpers.
//!
//! The engine treats the on-disk representation as the in-memory representation
//! for fixed-width little-endian data. These helpers wrap `bytemuck`'s
//! `cast_slice` / `cast_slice_mut` with a single additional invariant:
//! the input slice **must** be 64-byte aligned (which every minipage is, by
//! construction). This lets the storage layer hand out `&[i64]` / `&[u64]` /
//! `&[f64]` views over a frame's bytes with no per-element parsing, no
//! allocation, and no `unsafe` leaking into caller code.

use bytemuck::Pod;

/// Reinterpret a byte slice as a slice of `T`, where `T: Pod`.
///
/// Invariants enforced by `bytemuck`:
///   * the byte slice length must be a multiple of `size_of::<T>()`
///   * the alignment of `T` must divide the alignment of the input slice
///
/// Our additional guarantee (checked on debug builds): the byte slice is
/// 64-byte aligned. Every minipage we hand out satisfies this because
/// `PaxBlock` allocates the block buffer with `MINIPAGE_ALIGN` alignment and
/// every minipage offset is rounded up to a multiple of `MINIPAGE_ALIGN`.
#[inline]
pub fn cast_slice_bytes<T: Pod>(bytes: &[u8]) -> &[T] {
    debug_assert!(
        bytes.as_ptr() as usize % 64 == 0,
        "minipage must be 64-byte aligned, got ptr {:?}",
        bytes.as_ptr()
    );
    bytemuck::cast_slice(bytes)
}

/// Reinterpret a mutable byte slice as a mutable slice of `T`.
///
/// Same invariants as [`cast_slice_bytes`].
#[inline]
pub fn cast_slice_mut_bytes<T: Pod>(bytes: &mut [u8]) -> &mut [T] {
    debug_assert!(
        bytes.as_ptr() as usize % 64 == 0,
        "minipage must be 64-byte aligned, got ptr {:?}",
        bytes.as_ptr()
    );
    bytemuck::cast_slice_mut(bytes)
}

/// Read a single `Pod` value from a fixed offset in a byte buffer. The offset
/// must be aligned to `align_of::<T>()`.
#[inline]
pub fn pod_read_at<T: Pod>(buf: &[u8], off: usize) -> T {
    let end = off + core::mem::size_of::<T>();
    let slice = &buf[off..end];
    *bytemuck::from_bytes(slice)
}

/// Write a single `Pod` value to a fixed offset in a mutable byte buffer.
#[inline]
pub fn pod_write_at<T: Pod>(buf: &mut [u8], off: usize, val: &T) {
    let end = off + core::mem::size_of::<T>();
    let slice = &mut buf[off..end];
    slice.copy_from_slice(bytemuck::bytes_of(val));
}

/// Round `off` up to the next multiple of `align`. `align` must be a power of
/// two. Returns `off` unchanged if it is already aligned.
#[inline]
pub const fn align_up(off: usize, align: usize) -> usize {
    (off + align - 1) & !(align - 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pax::AlignedBlock;

    #[test]
    fn cast_works_on_aligned_buffer() {
        // AlignedBlock guarantees 64-byte alignment.
        let mut buf = AlignedBlock::zeroed(64).unwrap();
        let raw = buf.as_mut_slice();
        for (i, v) in [1i64, 2, 3, 4, 5, 6, 7, 8].iter().enumerate() {
            pod_write_at(raw, i * 8, v);
        }
        let view: &[i64] = cast_slice_bytes(buf.as_slice());
        assert_eq!(view, &[1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn align_up_is_correct() {
        assert_eq!(align_up(0, 64), 0);
        assert_eq!(align_up(1, 64), 64);
        assert_eq!(align_up(64, 64), 64);
        assert_eq!(align_up(65, 64), 128);
        assert_eq!(align_up(200, 64), 256);
    }
}
