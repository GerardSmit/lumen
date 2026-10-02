//! The linked engine's AOT compatibility and execution target.
pub use lumen_common::target::{Abi, Arch, Placement, Profile, TargetSpec};
pub use lumen_common::target::aarch64;

static CPU_FEATURES: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

/// Supply the system intersection before constructing any realm. Once read or
/// set, features cannot change while published JIT code may still execute.
pub fn set_cpu_features(features: u64) -> Result<(), &'static str> {
    if !cfg!(target_arch = "aarch64") && features != 0 {
        return Err("AArch64 features on a different architecture");
    }
    if features & !lumen_common::target::aarch64::ALL != 0 {
        return Err("unknown AArch64 CPU features");
    }
    CPU_FEATURES.set(features).map_err(|_| "CPU features already fixed")
}

pub fn cpu_features() -> u64 {
    *CPU_FEATURES.get_or_init(|| {
        #[cfg(all(target_arch = "aarch64", not(target_os = "none")))]
        {
            use lumen_common::target::aarch64::*;
            let mut features = 0;
            macro_rules! detect {
                ($name:literal, $bit:ident) => {
                    if std::arch::is_aarch64_feature_detected!($name) { features |= $bit; }
                };
            }
            detect!("lse", LSE);
            detect!("rcpc", RCPC);
            detect!("crc", CRC32);
            detect!("aes", AES);
            detect!("sha2", SHA2);
            detect!("jsconv", JSCVT);
            detect!("fp16", FP16);
            detect!("dotprod", DOTPROD);
            features
        }
        #[cfg(not(all(target_arch = "aarch64", not(target_os = "none"))))]
        { 0 }
    })
}

impl crate::Engine {
    pub fn target_spec(&self) -> TargetSpec {
        host()
    }
}

/// Describe the running engine using its frozen feature set. Native loading is
/// not yet linked; bytecode compatibility also checks the layout fingerprint.
pub fn host() -> TargetSpec {
    let (arch, abi, pointer_width) = if cfg!(target_arch = "aarch64") {
        (
            Arch::Aarch64,
            if cfg!(target_vendor = "apple") {
                Abi::Apple64
            } else {
                Abi::Aapcs64
            },
            64,
        )
    } else if cfg!(target_arch = "x86_64") {
        (
            Arch::X86_64,
            if cfg!(target_os = "windows") {
                Abi::Win64
            } else {
                Abi::SysV64
            },
            64,
        )
    } else if cfg!(target_arch = "wasm32") {
        (Arch::Wasm32, Abi::Wasm, 32)
    } else {
        panic!("AOT target description is unsupported on this architecture")
    };
    TargetSpec {
        lumen_version: lumen_common::aot::version_bytes(crate::precompiled::LUMEN_VERSION),
        bytecode_fp: crate::precompiled::LAYOUT_FINGERPRINT,
        native_fp: 0,
        arch,
        abi,
        pointer_width,
        features: cpu_features(),
        profile: Profile::Full,
        code_placement: Placement::Ram,
    }
}
