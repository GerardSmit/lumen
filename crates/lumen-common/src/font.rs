//! Language-neutral WOFF1/WOFF2 unpacking. Font parsing/rasterization stays
//! with the caller; all table lengths, ranges and decompression are checked here.

/// Restore a WOFF1 container into an sfnt font with the vendored `wuff` unpacker, inflating
/// tables with the shared bounded zlib decoder. `limit` bounds the total of the inflated tables
/// and the restored font.
pub fn decode_woff(bytes: &[u8], limit: usize) -> Result<Vec<u8>, &'static str> {
    if bytes.get(..4) != Some(b"wOFF") {
        return Err("not a WOFF1 font");
    }
    let mut budget = limit;
    let mut inflate = |packed: &[u8], expected: usize| {
        budget = budget.checked_sub(expected).ok_or_else(|| {
            Box::<dyn core::error::Error>::from("WOFF exceeds decoded font limit")
        })?;
        crate::compress::zlib_decompress_limited(packed, expected)
            .map_err(|_| Box::<dyn core::error::Error>::from("invalid compressed WOFF table"))
    };
    let sfnt = wuff::decompress_woff1_with_custom_z(bytes, &mut inflate)
        .map_err(|_| "invalid WOFF1 font")?;
    if sfnt.len() > limit {
        return Err("WOFF exceeds decoded font limit");
    }
    Ok(sfnt)
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
