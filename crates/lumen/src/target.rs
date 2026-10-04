//! The linked engine's AOT compatibility and execution target.
pub use lumen_common::target::aarch64;
pub use lumen_common::target::x64;
pub use lumen_common::target::{Abi, Arch, Placement, Profile, TargetSpec};

static CPU_FEATURES: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
static BUILTIN_MODULES_HASH: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

/// The only built-in native namespace supplied by the optional parallel feature.
pub const PARALLEL_BUILTIN_MODULES_HASH: u64 =
    lumen_common::aot::fingerprint::builtin_modules_hash(&[(
        "lumen:parallel",
        "",
        lumen_common::aot::fingerprint::binding_signature_hash("run/spawn/v1"),
    )]);
pub const EMPTY_BUILTIN_MODULES_HASH: u64 =
    lumen_common::aot::fingerprint::builtin_modules_hash(&[]);

/// The linked default catalog. The feature set determines which namespaces it
/// can install; a custom embedder may freeze another catalog before realm use.
pub const DEFAULT_BUILTIN_MODULES_HASH: u64 = if cfg!(feature = "parallel") {
    PARALLEL_BUILTIN_MODULES_HASH
} else {
    EMPTY_BUILTIN_MODULES_HASH
};

/// Freeze the host's built-in module/native table identity before realm creation.
pub fn set_builtin_modules_hash(hash: u64) -> Result<(), &'static str> {
    if hash == 0 {
        return Err("built-in module hash must be nonzero");
    }
    if let Some(existing) = BUILTIN_MODULES_HASH.get() {
        return if *existing == hash {
            Ok(())
        } else {
            Err("built-in module hash already fixed")
        };
    }
    match BUILTIN_MODULES_HASH.set(hash) {
        Ok(()) => Ok(()),
        Err(_) if BUILTIN_MODULES_HASH.get() == Some(&hash) => Ok(()),
        Err(_) => Err("built-in module hash already fixed"),
    }
}

/// Supply the system intersection before constructing any realm. Once read or
/// set, features cannot change while published JIT code may still execute.
pub fn set_cpu_features(features: u64) -> Result<(), &'static str> {
    let allowed = if cfg!(target_arch = "aarch64") {
        aarch64::ALL
    } else if cfg!(target_arch = "x86_64") {
        x64::ALL
    } else {
        0
    };
    if features & !allowed != 0 {
        return Err("unknown CPU features for architecture");
    }
    CPU_FEATURES
        .set(features)
        .map_err(|_| "CPU features already fixed")
}

pub fn cpu_features() -> u64 {
    *CPU_FEATURES.get_or_init(|| {
        #[cfg(all(target_arch = "aarch64", not(target_os = "none")))]
        {
            use lumen_common::target::aarch64::*;
            let mut features = 0;
            macro_rules! detect {
                ($name:tt, $bit:ident) => {
                    if std::arch::is_aarch64_feature_detected!($name) {
                        features |= $bit;
                    }
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
        #[cfg(all(target_arch = "x86_64", not(target_os = "none")))]
        {
            use lumen_common::target::x64::*;
            let mut features = 0;
            macro_rules! detect {
                ($name:tt, $bit:ident) => {
                    if std::arch::is_x86_feature_detected!($name) {
                        features |= $bit;
                    }
                };
            }
            detect!("lzcnt", LZCNT);
            detect!("bmi1", BMI1);
            detect!("bmi2", BMI2);
            detect!("popcnt", POPCNT);
            detect!("sse4.1", SSE41);
            features
        }
        #[cfg(not(all(
            any(target_arch = "aarch64", target_arch = "x86_64"),
            not(target_os = "none")
        )))]
        {
            0
        }
    })
}

impl crate::Engine {
    pub fn target_spec(&self) -> TargetSpec {
        host()
    }
}

/// Describe the running engine using its frozen feature set. A compilerless
/// profile publishes no bytecode fingerprint; native compatibility uses the
/// helper, frame, code, and built-in module table identity.
pub fn host() -> TargetSpec {
    let (arch, abi, pointer_width) = if cfg!(target_arch = "aarch64") {
        (
            Arch::Aarch64,
            if cfg!(target_vendor = "apple") {
                Abi::Apple64
            } else if cfg!(target_os = "windows") {
                Abi::Win64
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
    let mut target = TargetSpec {
        lumen_version: lumen_common::aot::version_bytes(crate::precompiled::LUMEN_VERSION),
        bytecode_fp: if cfg!(feature = "compiler") {
            crate::precompiled::LAYOUT_FINGERPRINT
        } else {
            0
        },
        native_fp: 0,
        arch,
        abi,
        pointer_width,
        page_size: lumen_os::jitmem::host_page_size(),
        // Compilerless images use the portable baseline so firmware-built
        // native glue and uploaded apps share one reproducible ABI identity.
        features: if cfg!(feature = "compiler") {
            cpu_features()
        } else {
            0
        },
        builtin_modules_hash: *BUILTIN_MODULES_HASH.get_or_init(|| DEFAULT_BUILTIN_MODULES_HASH),
        profile: if cfg!(feature = "jit") {
            Profile::Full
        } else if cfg!(feature = "compiler") {
            Profile::NoJit
        } else {
            Profile::Aot
        },
        code_placement: Placement::Ram,
    };
    #[cfg(feature = "aot-native")]
    if crate::native_aot::compiler_ready() {
        target.native_fp = crate::native_aot::fingerprint(&target);
    }
    target
}
