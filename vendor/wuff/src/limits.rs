use alloc::vec::Vec;
use core::mem::size_of;

use crate::error::WuffErr;

/// Allocate a vector only when its requested capacity fits the byte bound.
pub(crate) fn vec_with_capacity<T>(count: usize, max_bytes: usize) -> Result<Vec<T>, WuffErr> {
    let bytes = count
        .checked_mul(size_of::<T>())
        .ok_or(WuffErr::GenericError)?;
    if bytes > max_bytes {
        return Err(WuffErr::GenericError);
    }

    let mut values = Vec::new();
    values
        .try_reserve_exact(count)
        .map_err(|_| WuffErr::GenericError)?;
    if values
        .capacity()
        .checked_mul(size_of::<T>())
        .is_none_or(|allocated| allocated > max_bytes)
    {
        return Err(WuffErr::GenericError);
    }
    Ok(values)
}

/// Reserve enough room for additional vector elements without crossing a byte bound.
pub(crate) fn reserve<T>(
    values: &mut Vec<T>,
    additional: usize,
    max_bytes: usize,
) -> Result<(), WuffErr> {
    let count = values
        .len()
        .checked_add(additional)
        .ok_or(WuffErr::GenericError)?;
    let bytes = count
        .checked_mul(size_of::<T>())
        .ok_or(WuffErr::GenericError)?;
    if bytes > max_bytes {
        return Err(WuffErr::GenericError);
    }

    values
        .try_reserve_exact(additional)
        .map_err(|_| WuffErr::GenericError)?;
    if values
        .capacity()
        .checked_mul(size_of::<T>())
        .is_none_or(|allocated| allocated > max_bytes)
    {
        return Err(WuffErr::GenericError);
    }
    Ok(())
}

/// Return an aligned byte length while rejecting integer overflow or values above the bound.
pub(crate) fn aligned_len(
    len: usize,
    alignment: usize,
    max_bytes: usize,
) -> Result<usize, WuffErr> {
    let aligned = len
        .checked_add(alignment - 1)
        .ok_or(WuffErr::GenericError)?
        & !(alignment - 1);
    if aligned > max_bytes {
        return Err(WuffErr::GenericError);
    }
    Ok(aligned)
}

/// Check the final length before extending or resizing an output vector.
pub(crate) fn checked_end(
    current: usize,
    added: usize,
    max_bytes: usize,
) -> Result<usize, WuffErr> {
    let end = current.checked_add(added).ok_or(WuffErr::GenericError)?;
    if end > max_bytes {
        return Err(WuffErr::GenericError);
    }
    Ok(end)
}
