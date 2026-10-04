//! View descriptors: which bytes of a store a view covers and how they are laid out.

use super::format::{ByteOrder, ElemKind};
use super::BufferError;

/// The current element count of a 1-D contiguous view of `elsize`-byte elements at byte `offset`
/// in a buffer of `buf_len` bytes: `Some(len)` while a fixed-length view fits; for a
/// length-tracking view (`track`), as many whole elements as fit past `offset`. `None` = out of
/// bounds (the buffer shrank under the view).
#[inline(always)]
pub fn span_len(
    buf_len: usize,
    offset: usize,
    elsize: usize,
    len: usize,
    track: bool,
) -> Option<usize> {
    if track {
        if offset > buf_len {
            None
        } else {
            Some((buf_len - offset) / elsize)
        }
    } else {
        match len.checked_mul(elsize).and_then(|n| n.checked_add(offset)) {
            Some(end) if end <= buf_len => Some(len),
            _ => None,
        }
    }
}

/// The element count of `shape` (1 for no dimensions), or `None` when it exceeds `isize::MAX`
/// (Python's `SSIZE_MAX`). A zero-length dimension makes it 0 whatever the others are.
pub fn shape_product(shape: &[usize]) -> Option<usize> {
    if shape.contains(&0) {
        return Some(0);
    }
    shape.iter().try_fold(1usize, |acc, &n| {
        acc.checked_mul(n).filter(|&p| p <= isize::MAX as usize)
    })
}

/// C-order (row-major) strides for `shape`.
pub fn c_strides(shape: &[usize], itemsize: usize) -> Vec<isize> {
    let mut strides = vec![0isize; shape.len()];
    let mut step = itemsize as isize;
    for d in (0..shape.len()).rev() {
        strides[d] = step;
        step = step.saturating_mul(shape[d] as isize);
    }
    strides
}

/// Fortran-order (column-major) strides for `shape`.
pub fn f_strides(shape: &[usize], itemsize: usize) -> Vec<isize> {
    let mut strides = vec![0isize; shape.len()];
    let mut step = itemsize as isize;
    for d in 0..shape.len() {
        strides[d] = step;
        step = step.saturating_mul(shape[d] as isize);
    }
    strides
}

/// Python's `PySlice_Unpack` + `PySlice_AdjustIndices`: clamp `start`/`stop` (`None` = omitted,
/// meaning the end appropriate for the step's sign) against `len` and return
/// `(start, slice_len)`. `step` must be nonzero.
pub fn adjust_slice(
    len: usize,
    start: Option<isize>,
    stop: Option<isize>,
    step: isize,
) -> (isize, usize) {
    debug_assert!(step != 0);
    let len = len as isize;
    let clamp = |v: Option<isize>, default: isize| -> isize {
        match v {
            None => default,
            Some(v) if v < 0 => {
                let v = v + len;
                if v < 0 {
                    if step < 0 {
                        -1
                    } else {
                        0
                    }
                } else {
                    v
                }
            }
            Some(v) if v >= len => {
                if step < 0 {
                    len - 1
                } else {
                    len
                }
            }
            Some(v) => v,
        }
    };
    let (start, stop) = if step > 0 {
        (clamp(start, 0), clamp(stop, len))
    } else {
        (clamp(start, len - 1), clamp(stop, -1))
    };
    let n = if step > 0 {
        if stop > start {
            (stop - start - 1) / step + 1
        } else {
            0
        }
    } else if start > stop {
        (start - stop - 1) / (-step) + 1
    } else {
        0
    };
    (start, n as usize)
}

/// A shaped, strided, typed view of a store: what Python's `memoryview` (and the buffer protocol)
/// describes. A JS typed array or DataView is the 1-D contiguous case
/// ([`ViewDesc::contiguous`] with one dimension).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViewDesc {
    /// Byte offset of the element at index `[0, 0, ...]`.
    pub offset: usize,
    /// Bytes per element.
    pub itemsize: usize,
    /// The element kind; `None` for formats without element access (opaque `itemsize` bytes).
    pub elem: Option<ElemKind>,
    pub order: ByteOrder,
    pub shape: Vec<usize>,
    /// Byte step per dimension (may be negative, or zero for broadcast views).
    pub strides: Vec<isize>,
    pub readonly: bool,
}

impl ViewDesc {
    /// `len` unsigned bytes at `offset` (Python's default `memoryview` format `B`).
    pub fn bytes(offset: usize, len: usize, readonly: bool) -> ViewDesc {
        ViewDesc::contiguous(
            offset,
            Some(ElemKind::U8),
            1,
            ByteOrder::NATIVE,
            vec![len],
            readonly,
        )
    }

    /// A C-contiguous view of `shape` elements.
    pub fn contiguous(
        offset: usize,
        elem: Option<ElemKind>,
        itemsize: usize,
        order: ByteOrder,
        shape: Vec<usize>,
        readonly: bool,
    ) -> ViewDesc {
        let strides = c_strides(&shape, itemsize);
        ViewDesc {
            offset,
            itemsize,
            elem,
            order,
            shape,
            strides,
            readonly,
        }
    }

    pub fn ndim(&self) -> usize {
        self.shape.len()
    }

    /// Elements in the view (1 for a 0-dimensional view); `None` when the shape's product
    /// exceeds `isize::MAX`, which no view that passes [`check`](Self::check) can have.
    pub fn checked_nitems(&self) -> Option<usize> {
        shape_product(&self.shape)
    }

    /// Bytes the view's elements occupy if laid out contiguously; `None` past `isize::MAX`.
    pub fn checked_nbytes(&self) -> Option<usize> {
        self.checked_nitems()?
            .checked_mul(self.itemsize)
            .filter(|&n| n <= isize::MAX as usize)
    }

    /// [`checked_nitems`](Self::checked_nitems), saturating at `usize::MAX`: a view that large
    /// never fits a buffer, so [`check`](Self::check) rejects it before any element access.
    pub fn nitems(&self) -> usize {
        self.checked_nitems().unwrap_or(usize::MAX)
    }

    /// Bytes the view's elements occupy if laid out contiguously (`len(view.tobytes())`),
    /// saturating like [`nitems`](Self::nitems).
    pub fn nbytes(&self) -> usize {
        self.checked_nbytes().unwrap_or(usize::MAX)
    }

    pub fn is_c_contiguous(&self) -> bool {
        self.nitems() == 0 || self.packed(self.shape.iter().zip(&self.strides).rev())
    }

    pub fn is_f_contiguous(&self) -> bool {
        self.nitems() == 0 || self.packed(self.shape.iter().zip(&self.strides))
    }

    pub fn is_contiguous(&self) -> bool {
        self.is_c_contiguous() || self.is_f_contiguous()
    }

    /// Whether the dimensions, innermost first, step by the packed strides ([`c_strides`] /
    /// [`f_strides`]). Strides of length-1 dimensions never matter.
    fn packed<'a>(&self, dims: impl Iterator<Item = (&'a usize, &'a isize)>) -> bool {
        let mut step = self.itemsize as isize;
        for (&n, &s) in dims {
            if n > 1 && s != step {
                return false;
            }
            step = step.saturating_mul(n as isize);
        }
        true
    }

    /// The byte range `[lo, hi)` the view touches (`(offset, offset)` when empty); `None` when it
    /// reaches below byte 0 or overflows.
    pub fn extent(&self) -> Option<(usize, usize)> {
        self.checked_nbytes()?;
        if self.nitems() == 0 {
            return Some((self.offset, self.offset));
        }
        let mut lo = self.offset as isize;
        let mut hi = self.offset as isize;
        for (&n, &s) in self.shape.iter().zip(&self.strides) {
            let span = s.checked_mul(n as isize - 1)?;
            if span < 0 {
                lo = lo.checked_add(span)?;
            } else {
                hi = hi.checked_add(span)?;
            }
        }
        if lo < 0 {
            return None;
        }
        Some((lo as usize, (hi as usize).checked_add(self.itemsize)?))
    }

    /// Whether every element lies inside a buffer of `buf_len` bytes.
    pub fn check(&self, buf_len: usize) -> Result<(), BufferError> {
        match self.extent() {
            Some((_, hi)) if hi <= buf_len => Ok(()),
            _ => Err(BufferError::OutOfBounds),
        }
    }

    /// Normalize index `i` (negative counts from the end) along dimension `dim`.
    pub fn index(&self, dim: usize, i: isize) -> Option<usize> {
        let n = self.shape[dim] as isize;
        let i = if i < 0 { i + n } else { i };
        (0..n).contains(&i).then_some(i as usize)
    }

    /// Byte offset of the element at `idx` (one in-range index per dimension).
    pub fn item_offset(&self, idx: &[usize]) -> usize {
        debug_assert_eq!(idx.len(), self.ndim());
        let off = idx
            .iter()
            .zip(&self.strides)
            .fold(self.offset as isize, |a, (&i, &s)| a + i as isize * s);
        off as usize
    }

    /// The sub-view `start, start + step, ...` (`len` items) along `dim`; `start` is an in-range
    /// index when `len > 0` (see [`adjust_slice`]).
    pub fn slice(&self, dim: usize, start: isize, step: isize, len: usize) -> ViewDesc {
        let mut v = self.clone();
        let s = self.strides[dim];
        if len > 0 {
            v.offset = (self.offset as isize + start * s) as usize;
        }
        v.shape[dim] = len;
        v.strides[dim] = s * step;
        v
    }

    /// The view with dimension `dim` fixed at index `i`, dropping that dimension.
    pub fn select(&self, dim: usize, i: usize) -> ViewDesc {
        let mut v = self.clone();
        v.offset = (self.offset as isize + i as isize * self.strides[dim]) as usize;
        v.shape.remove(dim);
        v.strides.remove(dim);
        v
    }

    /// Reinterpret a C-contiguous view as `shape` (default: 1-D) elements of a new format. The
    /// byte count must be preserved. Format compatibility rules (Python only allows casts from
    /// or to a byte format) are the facade's.
    pub fn cast(
        &self,
        elem: Option<ElemKind>,
        itemsize: usize,
        order: ByteOrder,
        shape: Option<Vec<usize>>,
    ) -> Result<ViewDesc, CastError> {
        if !self.is_c_contiguous() {
            return Err(CastError::NotContiguous);
        }
        let nbytes = self.nbytes();
        let shape = match shape {
            Some(s) => {
                let n = shape_product(&s).ok_or(CastError::TooLarge)?;
                if n.checked_mul(itemsize) != Some(nbytes) {
                    return Err(CastError::SizeMismatch);
                }
                s
            }
            None => {
                if itemsize == 0 || !nbytes.is_multiple_of(itemsize) {
                    return Err(CastError::SizeMismatch);
                }
                vec![nbytes / itemsize]
            }
        };
        Ok(ViewDesc::contiguous(
            self.offset,
            elem,
            itemsize,
            order,
            shape,
            self.readonly,
        ))
    }

    /// Call `f` with the byte offset of every element, in C (row-major) order.
    pub fn for_each_offset(&self, mut f: impl FnMut(usize)) {
        if self.nitems() == 0 {
            return;
        }
        let nd = self.ndim();
        if nd == 0 {
            f(self.offset);
            return;
        }
        let mut idx = vec![0usize; nd];
        let mut off = self.offset as isize;
        loop {
            f(off as usize);
            let mut d = nd;
            loop {
                if d == 0 {
                    return;
                }
                d -= 1;
                idx[d] += 1;
                off += self.strides[d];
                if idx[d] < self.shape[d] {
                    break;
                }
                off -= self.strides[d] * self.shape[d] as isize;
                idx[d] = 0;
            }
        }
    }

    /// The view's bytes in C order (`memoryview.tobytes()`); `buf` must pass [`check`](Self::check).
    pub fn gather(&self, buf: &[u8]) -> Vec<u8> {
        if self.is_c_contiguous() {
            return buf[self.offset..self.offset + self.nbytes()].to_vec();
        }
        let mut out = Vec::with_capacity(self.nbytes());
        let n = self.itemsize;
        self.for_each_offset(|o| out.extend_from_slice(&buf[o..o + n]));
        out
    }

    /// Write `src` (C-order bytes, `nbytes()` long) into the view's positions in `buf`.
    pub fn scatter(&self, buf: &mut [u8], src: &[u8]) {
        debug_assert_eq!(src.len(), self.nbytes());
        if self.is_c_contiguous() {
            buf[self.offset..self.offset + src.len()].copy_from_slice(src);
            return;
        }
        let n = self.itemsize;
        let mut k = 0;
        self.for_each_offset(|o| {
            buf[o..o + n].copy_from_slice(&src[k..k + n]);
            k += n;
        });
    }
}

/// Why [`ViewDesc::cast`] refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CastError {
    NotContiguous,
    SizeMismatch,
    /// The requested shape has more than `isize::MAX` elements.
    TooLarge,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::format::load_f64;

    #[test]
    fn span_lengths() {
        assert_eq!(span_len(16, 4, 4, 3, false), Some(3));
        assert_eq!(span_len(15, 4, 4, 3, false), None);
        assert_eq!(span_len(15, 4, 4, 0, true), Some(2));
        assert_eq!(span_len(3, 4, 4, 0, true), None);
        assert_eq!(span_len(4, 4, 4, 0, true), Some(0));
    }

    #[test]
    fn strides_and_contiguity() {
        assert_eq!(c_strides(&[2, 3], 4), [12, 4]);
        assert_eq!(f_strides(&[2, 3], 4), [4, 8]);
        let v = ViewDesc::contiguous(
            0,
            Some(ElemKind::I32),
            4,
            ByteOrder::Little,
            vec![2, 3],
            false,
        );
        assert!(v.is_c_contiguous() && !v.is_f_contiguous());
        assert_eq!((v.nitems(), v.nbytes(), v.extent()), (6, 24, Some((0, 24))));
        let col = v.slice(1, 1, 1, 1);
        assert!(!col.is_c_contiguous() || col.shape[0] == 1);
        assert_eq!(col.shape, [2, 1]);
        let row = ViewDesc::bytes(3, 1, true);
        assert!(row.is_c_contiguous() && row.is_f_contiguous());
    }

    #[test]
    fn python_slice_adjustment() {
        // list(range(10))[2:8:3], [::-1], [8:2:-2], [-3:], [5:1]
        assert_eq!(adjust_slice(10, Some(2), Some(8), 3), (2, 2));
        assert_eq!(adjust_slice(10, None, None, -1), (9, 10));
        assert_eq!(adjust_slice(10, Some(8), Some(2), -2), (8, 3));
        assert_eq!(adjust_slice(10, Some(-3), None, 1), (7, 3));
        assert_eq!(adjust_slice(10, Some(5), Some(1), 1), (5, 0));
        assert_eq!(adjust_slice(0, None, None, -1), (-1, 0));
        assert_eq!(adjust_slice(4, Some(-100), Some(100), 1), (0, 4));
    }

    #[test]
    fn strided_gather_scatter_and_negative_steps() {
        let buf: Vec<u8> = (0..12).collect();
        let v = ViewDesc::contiguous(
            0,
            Some(ElemKind::U8),
            1,
            ByteOrder::Little,
            vec![3, 4],
            false,
        );
        // [:, ::2]
        let s = v.slice(1, 0, 2, 2);
        assert_eq!(s.gather(&buf), [0, 2, 4, 6, 8, 10]);
        // [::-1, 1]
        let (start, n) = adjust_slice(3, None, None, -1);
        let r = v.slice(0, start, -1, n).select(1, 1);
        assert_eq!(r.extent(), Some((1, 10)));
        assert_eq!(r.gather(&buf), [9, 5, 1]);
        r.check(12).unwrap();
        assert_eq!(r.check(9), Err(BufferError::OutOfBounds));
        let mut out = buf.clone();
        s.scatter(&mut out, &[100, 101, 102, 103, 104, 105]);
        assert_eq!(out, [100, 1, 101, 3, 102, 5, 103, 7, 104, 9, 105, 11]);
        assert_eq!(v.item_offset(&[2, 3]), 11);
        assert_eq!(v.index(1, -1), Some(3));
        assert_eq!(v.index(1, 4), None);
    }

    #[test]
    fn casts() {
        let v = ViewDesc::bytes(0, 8, false);
        let d = v
            .cast(Some(ElemKind::F64), 8, ByteOrder::Little, None)
            .unwrap();
        assert_eq!((d.shape.clone(), d.strides.clone()), (vec![1], vec![8]));
        let buf = 1.5f64.to_le_bytes();
        assert_eq!(
            load_f64(d.elem.unwrap(), &buf[d.item_offset(&[0])..], d.order),
            1.5
        );
        let m = v
            .cast(Some(ElemKind::U16), 2, ByteOrder::Little, Some(vec![2, 2]))
            .unwrap();
        assert_eq!(m.strides, [4, 2]);
        assert_eq!(
            v.cast(Some(ElemKind::I32), 4, ByteOrder::Little, Some(vec![3])),
            Err(CastError::SizeMismatch)
        );
        assert_eq!(
            ViewDesc::bytes(0, 7, false).cast(None, 2, ByteOrder::Little, None),
            Err(CastError::SizeMismatch)
        );
        assert_eq!(
            v.cast(None, 1, ByteOrder::Little, Some(vec![1 << 62, 4])),
            Err(CastError::TooLarge)
        );
        assert_eq!(
            v.cast(None, 8, ByteOrder::Little, Some(vec![1 << 61, 4])),
            Err(CastError::TooLarge)
        );
        assert_eq!(
            v.cast(None, 1 << 62, ByteOrder::Little, Some(vec![1 << 61])),
            Err(CastError::SizeMismatch)
        );
        let strided = v.slice(0, 0, 2, 4);
        assert_eq!(
            strided.cast(None, 1, ByteOrder::Little, None),
            Err(CastError::NotContiguous)
        );
    }

    #[test]
    fn zero_dim_and_empty_views() {
        let z = ViewDesc::contiguous(5, Some(ElemKind::U8), 1, ByteOrder::Little, vec![], false);
        assert_eq!(z.nitems(), 1);
        let mut seen = vec![];
        z.for_each_offset(|o| seen.push(o));
        assert_eq!(seen, [5]);
        let e = ViewDesc::bytes(3, 0, false);
        assert_eq!(e.extent(), Some((3, 3)));
        e.for_each_offset(|_| panic!("empty view has no elements"));
        let huge = ViewDesc::contiguous(0, None, 8, ByteOrder::Little, vec![1 << 62, 4, 0], false);
        assert_eq!((huge.nitems(), huge.extent()), (0, Some((0, 0))));
        let huge = ViewDesc::contiguous(0, None, 8, ByteOrder::Little, vec![1 << 62, 4], false);
        assert_eq!(
            (huge.checked_nitems(), huge.nbytes(), huge.extent()),
            (None, usize::MAX, None)
        );
        assert_eq!(huge.check(usize::MAX), Err(BufferError::OutOfBounds));
        assert_eq!(span_len(16, usize::MAX, 4, 3, false), None);
        assert_eq!(span_len(16, 4, 4, usize::MAX, false), None);
        assert_eq!(shape_product(&[]), Some(1));
    }
}
