//! Default sizes for memory a process keeps between uses (decoded-image caches, buffer pools,
//! parse and shaping caches), scaled to the device class.
//!
//! The desktop defaults assume gigabytes of RAM. 32-bit targets are small devices (Raspberry Pi
//! 1 with 256-512 MiB, 32-bit Android): there each retained-cache default is an eighth of the
//! desktop size. Hard caps on a single allocation (largest image, largest font) are limits, not
//! retained memory, and do not go through this. Embedders that know the device's memory can
//! still set each budget explicitly.

/// Whether this build targets a small-memory device class (32-bit pointers).
pub const SMALL_DEVICE: bool = cfg!(not(target_pointer_width = "64"));

/// The retained-cache budget for a cache whose desktop default is `desktop` bytes.
pub const fn retained(desktop: usize) -> usize {
    if SMALL_DEVICE { desktop / 8 } else { desktop }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retained_scales_only_on_small_devices() {
        let mib = 1024 * 1024;
        if SMALL_DEVICE {
            assert_eq!(retained(256 * mib), 32 * mib);
        } else {
            assert_eq!(retained(256 * mib), 256 * mib);
        }
    }
}
