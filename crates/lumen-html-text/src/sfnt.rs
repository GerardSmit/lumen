//! Big-endian readers and the one writer of a standalone sfnt table directory.

use crate::MAX_SYSTEM_FONT_BYTES;
use alloc::vec::Vec;

pub(crate) fn be16(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(bytes.get(at..at.checked_add(2)?)?.try_into().ok()?))
}

pub(crate) fn be32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.get(at..at.checked_add(4)?)?.try_into().ok()?))
}

pub(crate) fn write_be16(bytes: &mut [u8], at: usize, value: u16) -> Result<(), &'static str> {
    let end = at.checked_add(2).ok_or("invalid font table")?;
    bytes.get_mut(at..end).ok_or("invalid font table")?.copy_from_slice(&value.to_be_bytes());
    Ok(())
}

pub(crate) fn write_be32(bytes: &mut [u8], at: usize, value: u32) -> Result<(), &'static str> {
    let end = at.checked_add(4).ok_or("invalid font table")?;
    bytes.get_mut(at..end).ok_or("invalid font table")?.copy_from_slice(&value.to_be_bytes());
    Ok(())
}

pub(crate) fn checksum(bytes: &[u8]) -> u32 {
    bytes.chunks(4).fold(0u32, |sum, word| {
        let mut padded = [0; 4];
        padded[..word.len()].copy_from_slice(word);
        sum.wrapping_add(u32::from_be_bytes(padded))
    })
}

/// A standalone sfnt: a table directory of known size followed by 4-byte aligned table data.
pub(crate) struct SfntWriter {
    out: Vec<u8>,
}

impl SfntWriter {
    pub(crate) fn new(version: [u8; 4], table_count: usize) -> Result<Self, &'static str> {
        let count = u16::try_from(table_count).map_err(|_| "font table limit")?;
        let directory_end = table_count
            .checked_mul(16)
            .and_then(|bytes| bytes.checked_add(12))
            .ok_or("font table limit")?;
        let mut out = Vec::new();
        out.try_reserve(directory_end).map_err(|_| "font allocation failed")?;
        out.resize(directory_end, 0);
        out[..4].copy_from_slice(&version);
        write_be16(&mut out, 4, count)?;
        let mut power = 1u16;
        let mut selector = 0u16;
        while power.saturating_mul(2) <= count {
            power *= 2;
            selector += 1;
        }
        let search_range = power.saturating_mul(16);
        write_be16(&mut out, 6, search_range)?;
        write_be16(&mut out, 8, selector)?;
        write_be16(&mut out, 10, count.saturating_mul(16).saturating_sub(search_range))?;
        Ok(Self { out })
    }

    fn aligned_end(&self) -> Option<usize> {
        Some(self.out.len().checked_add(3)? & !3)
    }

    /// Whether a table of `length` bytes still fits the font size limit.
    pub(crate) fn fits(&self, length: usize) -> bool {
        self.aligned_end().and_then(|start| start.checked_add(length)).is_some_and(|end| end <= MAX_SYSTEM_FONT_BYTES)
    }

    /// Appends the table as directory entry `position` and returns its offset in the output.
    pub(crate) fn push_table(&mut self, position: usize, tag: [u8; 4], checksum: u32, data: &[u8]) -> Result<usize, &'static str> {
        if !self.fits(data.len()) {
            return Err("font too large");
        }
        let aligned = self.aligned_end().ok_or("font table limit")?;
        let entry = position.checked_mul(16).and_then(|bytes| bytes.checked_add(12)).ok_or("font table limit")?;
        if entry + 16 > self.out.len() {
            return Err("font table limit");
        }
        self.out.try_reserve(aligned + data.len() - self.out.len()).map_err(|_| "font allocation failed")?;
        self.out.resize(aligned, 0);
        self.out.extend_from_slice(data);
        self.out[entry..entry + 4].copy_from_slice(&tag);
        write_be32(&mut self.out, entry + 4, checksum)?;
        write_be32(&mut self.out, entry + 8, u32::try_from(aligned).map_err(|_| "font table limit")?)?;
        write_be32(&mut self.out, entry + 12, u32::try_from(data.len()).map_err(|_| "font table limit")?)?;
        Ok(aligned)
    }

    pub(crate) fn bytes_mut(&mut self) -> &mut [u8] {
        &mut self.out
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        &self.out
    }

    pub(crate) fn into_bytes(self) -> Vec<u8> {
        self.out
    }
}

#[cfg(feature = "system-fonts")]
/// The size of a standalone sfnt holding tables of the given lengths, as `SfntWriter` lays it out.
pub(crate) fn standalone_length(table_lengths: impl ExactSizeIterator<Item = u32>) -> u64 {
    let mut length = 12 + 16 * table_lengths.len() as u64;
    for table in table_lengths {
        length = ((length + 3) & !3) + u64::from(table);
    }
    length
}
