//! CRC-32 (IEEE 802.3, the zlib / gzip / PNG polynomial) over the maintained `crc32fast` crate,
//! available without any codec feature.

/// CRC-32 of `data`, continued from `seed` (0 for a fresh checksum, or the CRC of the bytes
/// before `data`).
#[inline]
pub fn crc32_from(seed: u32, data: &[u8]) -> u32 {
    let mut hasher = crc32fast::Hasher::new_with_initial(seed);
    hasher.update(data);
    hasher.finalize()
}

#[cfg(test)]
mod tests {
    use super::crc32_from;

    #[test]
    fn known_value_and_chaining() {
        assert_eq!(crc32_from(0, b"123456789"), 0xcbf4_3926);
        assert_eq!(crc32_from(crc32_from(0, b"1234"), b"56789"), 0xcbf4_3926);
    }
}
