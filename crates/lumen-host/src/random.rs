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

const POOL_BYTES: usize = 4096;

struct Pool {
    bytes: [u8; POOL_BYTES],
    next: usize,
}

thread_local! {
    static POOL: std::cell::RefCell<Pool> = const {
        std::cell::RefCell::new(Pool { bytes: [0; POOL_BYTES], next: POOL_BYTES })
    };
}

/// A random (version 4) UUID in canonical text form. Entropy comes from the OS in pooled 4 KiB
/// reads, so creating thousands of UUIDs does not cost a system call each.
pub fn random_uuid() -> Result<String, OpError> {
    let mut bytes = [0u8; 16];
    POOL.with(|pool| {
        let mut pool = pool.borrow_mut();
        if pool.next + bytes.len() > POOL_BYTES {
            fill_random(&mut pool.bytes)?;
            pool.next = 0;
        }
        bytes.copy_from_slice(&pool.bytes[pool.next..pool.next + 16]);
        pool.next += 16;
        Ok::<(), OpError>(())
    })?;
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

    #[test]
    fn pooled_uuids_stay_unique_across_pool_refills() {
        let all: std::collections::HashSet<String> = (0..3 * super::POOL_BYTES / 16 + 7)
            .map(|_| random_uuid().unwrap())
            .collect();
        assert_eq!(all.len(), 3 * super::POOL_BYTES / 16 + 7);
    }
}
