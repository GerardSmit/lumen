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
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    ))
}
