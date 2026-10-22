//! Randomness from the operating system, shared by Web Crypto and the Blob registry.
use lumen::embed::OpError;

/// Fill `buffer` from the operating system's CSPRNG.
pub fn fill_random(buffer: &mut [u8]) -> Result<(), OpError> {
    lumen_os::proc::entropy(buffer)
        .map_err(|error| OpError::error(format!("no randomness source: {error}")))
}

/// `n` cryptographically random bytes.
pub fn random_bytes(n: usize) -> Result<Vec<u8>, OpError> {
    let mut buffer = vec![0u8; n];
    fill_random(&mut buffer)?;
    Ok(buffer)
}

/// A random (version 4) UUID in canonical text form.
pub fn random_uuid() -> Result<String, OpError> {
    let mut bytes = [0u8; 16];
    fill_random(&mut bytes)?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(36);
    for (index, byte) in bytes.iter().enumerate() {
        if matches!(index, 4 | 6 | 8 | 10) {
            text.push('-');
        }
        text.push(DIGITS[usize::from(byte >> 4)] as char);
        text.push(DIGITS[usize::from(byte & 15)] as char);
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_is_canonical_version_four_text() {
        for _ in 0..64 {
            let uuid = random_uuid().unwrap();
            let bytes = uuid.as_bytes();
            assert_eq!(bytes.len(), 36);
            for index in [8, 13, 18, 23] {
                assert_eq!(bytes[index], b'-');
            }
            assert_eq!(bytes[14], b'4');
            assert!(matches!(bytes[19], b'8' | b'9' | b'a' | b'b'));
            assert!(uuid.bytes().all(|b| b == b'-' || b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
        }
    }
}
