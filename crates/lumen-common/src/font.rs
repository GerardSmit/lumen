//! Language-neutral WOFF1/WOFF2 unpacking. Font parsing/rasterization stays
//! with the caller; all table lengths, ranges and decompression are checked here.

fn u16_at(bytes: &[u8], at: usize) -> Result<u16, &'static str> {
    Ok(u16::from_be_bytes(
        bytes
            .get(at..at + 2)
            .ok_or("truncated WOFF")?
            .try_into()
            .unwrap(),
    ))
}
fn u32_at(bytes: &[u8], at: usize) -> Result<u32, &'static str> {
    Ok(u32::from_be_bytes(
        bytes
            .get(at..at + 4)
            .ok_or("truncated WOFF")?
            .try_into()
            .unwrap(),
    ))
}
fn aligned(size: usize) -> Result<usize, &'static str> {
    Ok(size.checked_add(3).ok_or("WOFF size overflow")? & !3)
}
fn checksum(bytes: &[u8], head: bool) -> u32 {
    bytes
        .chunks(4)
        .enumerate()
        .fold(0u32, |sum, (index, chunk)| {
            let mut word = [0; 4];
            word[..chunk.len()].copy_from_slice(chunk);
            sum.wrapping_add(if head && index == 2 {
                0
            } else {
                u32::from_be_bytes(word)
            })
        })
}

/// Restore a WOFF1 container into an sfnt font, using the existing bounded
/// zlib implementation. WOFF2 requires its transformed-table decoder and is
/// deliberately not treated as WOFF1.
pub fn decode_woff(bytes: &[u8], limit: usize) -> Result<Vec<u8>, &'static str> {
    if bytes.get(..4) != Some(b"wOFF") {
        return Err("not a WOFF1 font");
    }
    if u32_at(bytes, 8)? as usize != bytes.len() || u16_at(bytes, 14)? != 0 {
        return Err("invalid WOFF header");
    }
    let count = u16_at(bytes, 12)? as usize;
    if count == 0 || count > 4095 {
        return Err("invalid WOFF table count");
    }
    let directory_end = 44 + count * 20;
    bytes
        .get(..directory_end)
        .ok_or("truncated WOFF directory")?;
    let total = u32_at(bytes, 16)? as usize;
    if total > limit || total < 12 + count * 16 || total % 4 != 0 {
        return Err("WOFF exceeds decoded font limit");
    }
    let mut ranges = Vec::with_capacity(count + 2);
    let mut tables = Vec::with_capacity(count);
    let mut expected = 12 + count * 16;
    let mut previous_tag = None;
    for index in 0..count {
        let at = 44 + index * 20;
        let tag = u32_at(bytes, at)?;
        if previous_tag.is_some_and(|previous| previous >= tag) {
            return Err("unsorted or duplicate WOFF table tag");
        }
        previous_tag = Some(tag);
        let start = u32_at(bytes, at + 4)? as usize;
        let compressed = u32_at(bytes, at + 8)? as usize;
        let original = u32_at(bytes, at + 12)? as usize;
        let end = start
            .checked_add(compressed)
            .ok_or("WOFF table range overflow")?;
        if start < directory_end || start % 4 != 0 || compressed > original || original > limit {
            return Err("invalid WOFF table length or offset");
        }
        bytes.get(start..end).ok_or("truncated WOFF table")?;
        expected = expected
            .checked_add(aligned(original)?)
            .ok_or("WOFF size overflow")?;
        if expected > total {
            return Err("invalid WOFF sfnt size");
        }
        ranges.push((start, end, 0u8));
        tables.push((tag, start, compressed, original, u32_at(bytes, at + 16)?));
    }
    if expected != total {
        return Err("invalid WOFF sfnt size");
    }
    for (offset_at, length_at, metadata) in [(24, 28, true), (36, 40, false)] {
        let start = u32_at(bytes, offset_at)? as usize;
        let length = u32_at(bytes, length_at)? as usize;
        if start == 0 {
            if length != 0 || metadata && u32_at(bytes, 32)? != 0 {
                return Err("invalid absent WOFF auxiliary block");
            }
        } else {
            let end = start
                .checked_add(length)
                .ok_or("WOFF block range overflow")?;
            if start < directory_end || start % 4 != 0 || length == 0 {
                return Err("invalid WOFF auxiliary block");
            }
            bytes
                .get(start..end)
                .ok_or("truncated WOFF auxiliary block")?;
            ranges.push((start, end, if metadata { 1 } else { 2 }));
        }
    }
    ranges.sort_unstable();
    let mut previous_end = directory_end;
    let mut previous_kind = 0;
    for &(start, end, kind) in &ranges {
        if kind < previous_kind
            || start != aligned(previous_end)?
            || bytes[previous_end..start].iter().any(|byte| *byte != 0)
        {
            return Err("overlapping or noncontiguous WOFF data");
        }
        previous_end = end;
        previous_kind = kind;
    }
    let last_is_table = tables
        .iter()
        .any(|(_, start, len, _, _)| *start + *len == previous_end);
    let final_end = if last_is_table {
        aligned(previous_end)?
    } else {
        previous_end
    };
    if final_end != bytes.len() || bytes[previous_end..].iter().any(|byte| *byte != 0) {
        return Err("extraneous WOFF data");
    }
    let mut out = vec![0; total];
    out[..4].copy_from_slice(&bytes[4..8]);
    out[4..6].copy_from_slice(&(count as u16).to_be_bytes());
    let power = 1usize << (usize::BITS - 1 - count.leading_zeros());
    out[6..8].copy_from_slice(&((power * 16) as u16).to_be_bytes());
    out[8..10].copy_from_slice(&(power.trailing_zeros() as u16).to_be_bytes());
    out[10..12].copy_from_slice(&((count * 16 - power * 16) as u16).to_be_bytes());
    let mut write_at = 12 + count * 16;
    let mut head_offset = None;
    for (index, (tag, start, compressed, original, sum)) in tables.into_iter().enumerate() {
        let packed = &bytes[start..start + compressed];
        let decoded;
        let table = if compressed < original {
            decoded = crate::compress::zlib_decompress_limited(packed, original)
                .map_err(|_| "invalid compressed WOFF table")?;
            if decoded.len() != original {
                return Err("invalid WOFF table decoded length");
            }
            decoded.as_slice()
        } else {
            packed
        };
        let head = tag == u32::from_be_bytes(*b"head");
        if checksum(table, head) != sum {
            return Err("invalid WOFF table checksum");
        }
        let record = 12 + index * 16;
        out[record..record + 4].copy_from_slice(&tag.to_be_bytes());
        out[record + 4..record + 8].copy_from_slice(&sum.to_be_bytes());
        out[record + 8..record + 12].copy_from_slice(&(write_at as u32).to_be_bytes());
        out[record + 12..record + 16].copy_from_slice(&(original as u32).to_be_bytes());
        out[write_at..write_at + original].copy_from_slice(table);
        if head {
            if original < 12 {
                return Err("truncated WOFF head table");
            }
            head_offset = Some(write_at + 8);
            out[write_at + 8..write_at + 12].fill(0);
        }
        write_at += aligned(original)?;
    }
    if let Some(head) = head_offset {
        let adjustment = 0xB1B0_AFBAu32.wrapping_sub(checksum(&out, false));
        out[head..head + 4].copy_from_slice(&adjustment.to_be_bytes());
    }
    Ok(out)
}

#[derive(Debug)]
struct FontBrotliError;

impl core::fmt::Display for FontBrotliError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("bounded font Brotli decode failed")
    }
}

impl core::error::Error for FontBrotliError {}

/// Decode a WOFF2 font into sfnt form using the shared bounded Brotli decoder.
///
/// The same ceiling applies to WOFF2's decoded table stream, reconstruction
/// scratch and final sfnt output. The vendor decoder does not use the advisory
/// `totalSfntSize` or transformed `origLength` fields as allocation limits.
pub fn decode_woff2(bytes: &[u8], limit: usize) -> Result<Vec<u8>, &'static str> {
    if bytes.get(..4) != Some(b"wOF2") {
        return Err("not a WOFF2 font");
    }
    let mut decompress_brotli = |compressed: &[u8], expected: usize| {
        if expected > limit {
            return Err(Box::new(FontBrotliError) as Box<dyn core::error::Error>);
        }
        crate::compress::brotli_decompress_limited(compressed, expected)
            .map_err(|_| Box::new(FontBrotliError) as Box<dyn core::error::Error>)
    };
    wuff::decompress_woff2_with_custom_brotli_limited(bytes, limit, &mut decompress_brotli)
        .map_err(|_| "invalid WOFF2 font")
}

#[cfg(test)]
mod tests {
    use super::*;

    const FONT_LIMIT: usize = 2 * 1024 * 1024;

    #[test]
    fn bounded_woff2_decoder_accepts_plain_and_transformed_fixtures() {
        for bytes in [
            &include_bytes!("../../../vendor/wuff/tests/fixtures/valid-001.woff2")[..],
            &include_bytes!(
                "../../../vendor/wuff/tests/fixtures/tabledata-glyf-origlength-003.woff2"
            )[..],
        ] {
            let sfnt = decode_woff2(bytes, FONT_LIMIT).unwrap();
            assert!(sfnt.len() >= 12);
            assert!(!sfnt.starts_with(b"wOF2"));
            assert!(decode_woff2(bytes, sfnt.len() - 1).is_err());
        }
    }

    #[test]
    fn bounded_woff2_decoder_rejects_invalid_ranges_and_short_input() {
        let valid = include_bytes!("../../../vendor/wuff/tests/fixtures/valid-001.woff2");
        let overlap =
            include_bytes!("../../../vendor/wuff/tests/fixtures/blocks-overlap-002.woff2");
        assert!(decode_woff2(overlap, FONT_LIMIT).is_err());
        assert!(decode_woff2(&valid[..valid.len() - 1], FONT_LIMIT).is_err());
        assert!(decode_woff2(valid, 1).is_err());
        assert!(decode_woff2(b"not a font", FONT_LIMIT).is_err());
    }
}
