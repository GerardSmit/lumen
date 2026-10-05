//! Select a native backend from the requested target, independently of the host.

use crate::{aarch64, aot_image, x64, Function};
use lumen_common::target::{Abi, Arch, TargetSpec};

/// Compile with the target's ABI and portable CPU baseline or declared AArch64
/// features. Current backend relocations still patch code and are not AOT-MC GOT
/// relocations; the result must not be serialized as a native-only blob.
pub fn compile(
    func: &Function,
    target: &TargetSpec,
    traps: Option<x64::TrapConfig>,
) -> Result<x64::Compiled, String> {
    target.validate().map_err(str::to_owned)?;
    if target.native_fp == 0 {
        return Err("target does not advertise a native ABI".into());
    }
    match (target.arch, target.abi) {
        (Arch::Aarch64, Abi::Aapcs64 | Abi::Apple64 | Abi::Win64) => {
            let abi = match target.abi {
                Abi::Apple64 => aarch64::regs::apple(),
                Abi::Win64 => aarch64::regs::windows(),
                _ => aarch64::regs::aapcs64(),
            };
            aarch64::compile(
                func,
                &aarch64::Config {
                    abi,
                    traps,
                    features: target.features,
                },
            )
        }
        (Arch::X86_64, Abi::SysV64 | Abi::Win64) => {
            let abi = if target.abi == Abi::Win64 {
                x64::regs::win64()
            } else {
                x64::regs::sysv()
            };
            x64::compile(
                func,
                &x64::Config {
                    abi,
                    features: x64::Features::from_bits(target.features),
                    traps,
                },
            )
        }
        _ => Err("native backend unavailable for target".into()),
    }
}

/// Compile a function for a native-only image. Direct calls and `symbol_addr`
/// loads use GOT slots;
/// [`aot_image::link`] fixes their PC-relative offsets before serialization.
/// `symbol` maps backend import ids to GOT kinds and indices.
pub fn compile_aot(
    func: &Function,
    target: &TargetSpec,
    traps: Option<x64::TrapConfig>,
    symbol: impl Fn(u32) -> Option<(lumen_common::aot::got::Kind, u32)>,
) -> Result<aot_image::Function, String> {
    let mut optimized = func.clone();
    crate::opt::optimize_for_size(&mut optimized);
    let compiled = compile(&optimized, target, traps)?;
    aot_image::function(target, compiled, symbol)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FunctionBuilder, Signature, Type};
    use lumen_common::target::{Placement, Profile};

    #[test]
    fn host_compiles_for_requested_architecture_and_abi() {
        let mut func = Function::new("identity", Signature::new(vec![Type::I64], vec![Type::I64]));
        let mut builder = FunctionBuilder::new(&mut func);
        let entry = builder.create_entry_block();
        let value = builder.block_params(entry)[0];
        builder.ret(&[value]);
        builder.finish();

        let mut target = TargetSpec {
            lumen_version: [1; 16],
            bytecode_fp: 0,
            native_fp: 1,
            arch: Arch::Aarch64,
            abi: Abi::Aapcs64,
            pointer_width: 64,
            page_size: 4096,
            features: 0,
            builtin_modules_hash: 0,
            profile: Profile::Aot,
            code_placement: Placement::Ram,
        };
        let arm = compile(&func, &target, None).unwrap();
        assert!(!arm.code.is_empty());
        target.abi = Abi::Apple64;
        assert!(!compile(&func, &target, None).unwrap().code.is_empty());
        target.arch = Arch::X86_64;
        target.abi = Abi::SysV64;
        let x64 = compile(&func, &target, None).unwrap();
        assert!(!x64.code.is_empty());
        assert_ne!(arm.code, x64.code);
        target.abi = Abi::Win64;
        assert!(!compile(&func, &target, None).unwrap().code.is_empty());
        target.arch = Arch::Wasm32;
        target.abi = Abi::Wasm;
        target.pointer_width = 32;
        assert!(compile(&func, &target, None).is_err());

        target.arch = Arch::Aarch64;
        target.abi = Abi::Aapcs64;
        target.pointer_width = 64;
        target.profile = Profile::Full;
        target.bytecode_fp = 1;
        target.native_fp = 0;
        assert!(compile(&func, &target, None).is_err());
    }
}
