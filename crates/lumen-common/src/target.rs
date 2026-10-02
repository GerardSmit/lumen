//! Stable target description shared by AOT producers and device handshakes.

/// Stable AArch64 feature bits used by embedders, codegen and AOT metadata.
pub mod aarch64 {
    pub const LSE: u64 = 1 << 0;
    pub const RCPC: u64 = 1 << 1;
    pub const CRC32: u64 = 1 << 2;
    pub const AES: u64 = 1 << 3;
    pub const SHA2: u64 = 1 << 4;
    pub const JSCVT: u64 = 1 << 5;
    pub const FP16: u64 = 1 << 6;
    pub const DOTPROD: u64 = 1 << 7;
    pub const ALL: u64 = (1 << 8) - 1;
}

/// Stable x86-64 extensions above the architectural SSE2 baseline.
/// Bit meanings are architecture-specific; AVX state is not required by these paths.
pub mod x64 {
    pub const LZCNT: u64 = 1 << 0;
    pub const BMI1: u64 = 1 << 1;
    pub const BMI2: u64 = 1 << 2;
    pub const POPCNT: u64 = 1 << 3;
    pub const SSE41: u64 = 1 << 4;
    pub const ALL: u64 = (1 << 5) - 1;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Arch {
    Aarch64 = 1,
    X86_64 = 2,
    Wasm32 = 3,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Abi {
    Aapcs64 = 1,
    Apple64 = 2,
    SysV64 = 3,
    Win64 = 4,
    Wasm = 5,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Profile {
    Full = 1,
    NoJit = 2,
    Aot = 3,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    Ram,
    ExecuteInPlace { base: u64, len: u64 },
}

/// Fingerprints are supplied by the language engine, never inferred from a board name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TargetSpec {
    pub lumen_version: [u8; 16],
    pub bytecode_fp: u64,
    pub native_fp: u64,
    pub arch: Arch,
    pub abi: Abi,
    pub pointer_width: u8,
    /// Runtime memory page size used for the native code/GOT boundary.
    pub page_size: u32,
    pub features: u64,
    /// Hash of built-in module names and native signatures available on device.
    pub builtin_modules_hash: u64,
    pub profile: Profile,
    pub code_placement: Placement,
}

impl TargetSpec {
    pub const ENCODED_LEN: usize = 88;

    /// Check a blob's requirements against the executing engine.
    pub fn supported_by(&self, host: &Self) -> Result<(), &'static str> {
        self.validate()?;
        host.validate()?;
        if self.lumen_version != host.lumen_version
            || self.bytecode_fp != host.bytecode_fp
            || self.native_fp != host.native_fp
        {
            return Err("target version or layout fingerprint mismatch");
        }
        if self.arch != host.arch
            || self.abi != host.abi
            || self.pointer_width != host.pointer_width
        {
            return Err("target architecture or ABI mismatch");
        }
        if self.page_size != host.page_size
            || self.builtin_modules_hash != host.builtin_modules_hash
        {
            return Err("target page size or built-in modules mismatch");
        }
        if self.features & !host.features != 0 {
            return Err("target requires unavailable CPU features");
        }
        if self.code_placement != host.code_placement || self.profile != host.profile {
            return Err("target profile or code placement mismatch");
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        let valid = match self.arch {
            Arch::Aarch64 => {
                self.pointer_width == 64
                    && matches!(self.abi, Abi::Aapcs64 | Abi::Apple64 | Abi::Win64)
            }
            Arch::X86_64 => {
                self.pointer_width == 64 && matches!(self.abi, Abi::SysV64 | Abi::Win64)
            }
            Arch::Wasm32 => self.pointer_width == 32 && self.abi == Abi::Wasm,
        };
        if !valid {
            return Err("inconsistent architecture, ABI or pointer width");
        }
        if self.page_size < 4096 || self.page_size > 65536 || !self.page_size.is_power_of_two() {
            return Err("invalid native page size");
        }
        let allowed = match self.arch {
            Arch::Aarch64 => aarch64::ALL,
            Arch::X86_64 => x64::ALL,
            Arch::Wasm32 => 0,
        };
        if self.features & !allowed != 0 {
            return Err("unknown CPU features for architecture");
        }
        match self.profile {
            Profile::Aot if self.bytecode_fp != 0 || self.native_fp == 0 => {
                return Err("invalid AOT profile fingerprints")
            }
            Profile::Full | Profile::NoJit if self.bytecode_fp == 0 => {
                return Err("missing bytecode fingerprint")
            }
            _ => {}
        }
        if let Placement::ExecuteInPlace { base, len } = self.code_placement {
            if len == 0
                || base.checked_add(len).is_none()
                || base % u64::from(self.page_size) != 0
                || len % u64::from(self.page_size) != 0
            {
                return Err("invalid execute-in-place range");
            }
        }
        Ok(())
    }

    /// Fixed-width little-endian format, independent of Rust enum and struct layouts.
    pub fn encode(&self) -> Result<[u8; Self::ENCODED_LEN], &'static str> {
        self.validate()?;
        let mut b = [0; Self::ENCODED_LEN];
        b[..8].copy_from_slice(b"LUMTGT02");
        b[8..24].copy_from_slice(&self.lumen_version);
        b[24..32].copy_from_slice(&self.bytecode_fp.to_le_bytes());
        b[32..40].copy_from_slice(&self.native_fp.to_le_bytes());
        b[40] = self.arch as u8;
        b[41] = self.abi as u8;
        b[42] = self.pointer_width;
        b[43] = self.profile as u8;
        b[48..56].copy_from_slice(&self.features.to_le_bytes());
        if let Placement::ExecuteInPlace { base, len } = self.code_placement {
            b[44] = 1;
            b[56..64].copy_from_slice(&base.to_le_bytes());
            b[64..72].copy_from_slice(&len.to_le_bytes());
        }
        b[72..76].copy_from_slice(&self.page_size.to_le_bytes());
        b[80..88].copy_from_slice(&self.builtin_modules_hash.to_le_bytes());
        Ok(b)
    }

    pub fn decode(b: &[u8]) -> Result<Self, &'static str> {
        let legacy = b.len() == 72 && &b[..8] == b"LUMTGT01";
        if !legacy && (b.len() != Self::ENCODED_LEN || &b[..8] != b"LUMTGT02") {
            return Err("invalid target header");
        }
        if b[45..48] != [0; 3] || (!legacy && b[76..80] != [0; 4]) {
            return Err("unsupported target flags");
        }
        let u64_at = |at| u64::from_le_bytes(b[at..at + 8].try_into().unwrap());
        if legacy && u64_at(32) != 0 {
            return Err("legacy target cannot advertise native ABI");
        }
        let abi = match b[41] {
            1 => Abi::Aapcs64,
            2 => Abi::Apple64,
            3 => Abi::SysV64,
            4 => Abi::Win64,
            5 => Abi::Wasm,
            _ => return Err("unknown ABI"),
        };
        let spec = Self {
            lumen_version: b[8..24].try_into().unwrap(),
            bytecode_fp: u64_at(24),
            native_fp: u64_at(32),
            arch: match b[40] {
                1 => Arch::Aarch64,
                2 => Arch::X86_64,
                3 => Arch::Wasm32,
                _ => return Err("unknown architecture"),
            },
            abi,
            pointer_width: b[42],
            page_size: if legacy {
                match abi {
                    Abi::Apple64 => 16384,
                    Abi::Wasm => 65536,
                    _ => 4096,
                }
            } else {
                u32::from_le_bytes(b[72..76].try_into().unwrap())
            },
            profile: match b[43] {
                1 => Profile::Full,
                2 => Profile::NoJit,
                3 => Profile::Aot,
                _ => return Err("unknown profile"),
            },
            features: u64_at(48),
            builtin_modules_hash: if legacy { 0 } else { u64_at(80) },
            code_placement: match b[44] {
                0 if u64_at(56) == 0 && u64_at(64) == 0 => Placement::Ram,
                1 => Placement::ExecuteInPlace {
                    base: u64_at(56),
                    len: u64_at(64),
                },
                _ => return Err("invalid code placement"),
            },
        };
        spec.validate()?;
        Ok(spec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> TargetSpec {
        TargetSpec {
            lumen_version: crate::aot::version_bytes("0.1.0"),
            bytecode_fp: 42,
            native_fp: 0,
            arch: Arch::Aarch64,
            abi: Abi::Aapcs64,
            pointer_width: 64,
            page_size: 4096,
            features: 0,
            builtin_modules_hash: 0,
            profile: Profile::Full,
            code_placement: Placement::Ram,
        }
    }

    #[test]
    fn roundtrip() {
        for placement in [
            Placement::Ram,
            Placement::ExecuteInPlace {
                base: 0x1000,
                len: 4096,
            },
        ] {
            let s = TargetSpec {
                code_placement: placement,
                ..spec()
            };
            let b = s.encode().unwrap();
            assert_eq!(TargetSpec::decode(&b).unwrap(), s);
            for n in 0..b.len() {
                assert!(TargetSpec::decode(&b[..n]).is_err());
            }
        }
        let encoded = spec().encode().unwrap();
        let mut legacy = [0; 72];
        legacy.copy_from_slice(&encoded[..72]);
        legacy[..8].copy_from_slice(b"LUMTGT01");
        assert_eq!(TargetSpec::decode(&legacy).unwrap(), spec());
    }

    #[test]
    fn rejects_invalid_specs_and_wire_tags() {
        let b = spec().encode().unwrap();
        for at in [40, 41, 42, 43, 44, 45, 56] {
            let mut bad = b;
            bad[at] = 255;
            assert!(
                TargetSpec::decode(&bad).is_err(),
                "accepted invalid byte {at}"
            );
        }
        assert!(TargetSpec {
            profile: Profile::Aot,
            ..spec()
        }
        .encode()
        .is_err());
        assert!(TargetSpec {
            code_placement: Placement::ExecuteInPlace {
                base: u64::MAX,
                len: 1
            },
            ..spec()
        }
        .encode()
        .is_err());
        assert!(TargetSpec {
            abi: Abi::SysV64,
            ..spec()
        }
        .encode()
        .is_err());
        assert!(TargetSpec {
            features: aarch64::ALL + 1,
            ..spec()
        }
        .encode()
        .is_err());
        assert!(TargetSpec {
            arch: Arch::X86_64,
            abi: Abi::SysV64,
            features: x64::ALL + 1,
            ..spec()
        }
        .encode()
        .is_err());
    }

    #[test]
    fn x64_features_roundtrip_and_require_an_available_subset() {
        let host = TargetSpec {
            arch: Arch::X86_64,
            abi: Abi::Win64,
            features: x64::ALL,
            ..spec()
        };
        assert_eq!(TargetSpec::decode(&host.encode().unwrap()).unwrap(), host);
        let required = TargetSpec {
            features: x64::BMI2 | x64::SSE41,
            ..host
        };
        assert!(required.supported_by(&host).is_ok());
        assert!(required
            .supported_by(&TargetSpec {
                features: x64::BMI2,
                ..host
            })
            .is_err());
    }
}
