//! Native code compatibility identity, shared by host compilers and device loaders.

use crate::target::{Abi, Arch};

#[derive(Clone, Copy)]
pub struct Layout<'a> {
    pub size: u32,
    pub align: u32,
    pub fields: &'a [(&'a str, u32)],
}

/// Helper names and signatures are hashed in table order; static names likewise.
#[derive(Clone, Copy)]
pub struct NativeAbi<'a> {
    pub arch: Arch,
    pub abi: Abi,
    pub features: u64,
    pub pointer_width: u8,
    pub page_size: u32,
    pub builtin_modules_hash: u64,
    pub value_layout: Layout<'a>,
    pub frame_layout: Layout<'a>,
    pub helpers: &'a [(&'a str, &'a str)],
    pub statics: &'a [&'a str],
    pub data_version: u32,
    pub code_version: u32,
}

use crate::fasthash::{fnv1a64 as bytes, FNV1A64_OFFSET as OFFSET};

const fn u32_field(hash: u64, value: u32) -> u64 {
    bytes(hash, &value.to_le_bytes())
}

const fn u64_field(hash: u64, value: u64) -> u64 {
    bytes(hash, &value.to_le_bytes())
}

const fn str_field(hash: u64, value: &str) -> u64 {
    let hash = u64_field(hash, value.len() as u64);
    bytes(hash, value.as_bytes())
}

const fn layout(mut hash: u64, value: &Layout<'_>) -> u64 {
    hash = u32_field(hash, value.size);
    hash = u32_field(hash, value.align);
    hash = u64_field(hash, value.fields.len() as u64);
    let mut i = 0;
    while i < value.fields.len() {
        hash = str_field(hash, value.fields[i].0);
        hash = u32_field(hash, value.fields[i].1);
        i += 1;
    }
    hash
}

/// FNV-1a compatibility check, not a signature or a security boundary.
/// Each variable-length field is length-prefixed to avoid ambiguous concatenations.
pub const fn calculate(input: &NativeAbi<'_>) -> u64 {
    let mut hash = bytes(OFFSET, b"LUMEN-NATIVE-ABI-1");
    hash = bytes(
        hash,
        &[input.arch as u8, input.abi as u8, input.pointer_width],
    );
    hash = bytes(hash, &input.features.to_le_bytes());
    hash = u32_field(hash, input.page_size);
    hash = u64_field(hash, input.builtin_modules_hash);
    hash = layout(hash, &input.value_layout);
    hash = layout(hash, &input.frame_layout);
    hash = u64_field(hash, input.helpers.len() as u64);
    let mut i = 0;
    while i < input.helpers.len() {
        hash = str_field(hash, input.helpers[i].0);
        hash = str_field(hash, input.helpers[i].1);
        i += 1;
    }
    hash = u64_field(hash, input.statics.len() as u64);
    i = 0;
    while i < input.statics.len() {
        hash = str_field(hash, input.statics[i]);
        i += 1;
    }
    hash = u32_field(hash, input.data_version);
    hash = u32_field(hash, input.code_version);
    if hash == 0 {
        1
    } else {
        hash
    }
}

/// Stable identity of the ordered built-in module/native registration table.
/// A module without natives uses an empty native name and signature hash zero.
pub const fn builtin_modules_hash(entries: &[(&str, &str, u64)]) -> u64 {
    let mut hash = bytes(OFFSET, b"LUMEN-BUILTIN-TABLE-1");
    hash = u64_field(hash, entries.len() as u64);
    let mut i = 0;
    while i < entries.len() {
        hash = str_field(hash, entries[i].0);
        hash = str_field(hash, entries[i].1);
        hash = u64_field(hash, entries[i].2);
        i += 1;
    }
    if hash == 0 {
        1
    } else {
        hash
    }
}

/// Identity of a `lumen-bind` declaration's native calling contract.
pub const fn binding_signature_hash(signature: &str) -> u64 {
    let hash = str_field(bytes(OFFSET, b"LUMEN-BIND-SIGNATURE-1"), signature);
    if hash == 0 {
        1
    } else {
        hash
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: NativeAbi<'static> = NativeAbi {
        arch: Arch::Aarch64,
        abi: Abi::Aapcs64,
        features: 0,
        pointer_width: 64,
        page_size: 4096,
        builtin_modules_hash: 0,
        value_layout: Layout {
            size: 16,
            align: 8,
            fields: &[("tag", 0), ("payload", 8)],
        },
        frame_layout: Layout {
            size: 48,
            align: 16,
            fields: &[("slots", 0), ("consts", 8), ("stack", 16)],
        },
        helpers: &[("op_add", "(Value,Value)->Value")],
        statics: &["undefined"],
        data_version: 1,
        code_version: 1,
    };

    #[test]
    fn fingerprint_covers_every_component() {
        const EXPECTED: u64 = calculate(&BASE);
        assert_ne!(EXPECTED, 0);
        let mut other = BASE;
        other.arch = Arch::X86_64;
        assert_ne!(calculate(&other), EXPECTED);
        other = BASE;
        other.abi = Abi::Apple64;
        assert_ne!(calculate(&other), EXPECTED);
        other = BASE;
        other.features = 1;
        assert_ne!(calculate(&other), EXPECTED);
        other = BASE;
        other.pointer_width = 32;
        assert_ne!(calculate(&other), EXPECTED);
        other = BASE;
        other.page_size = 16384;
        assert_ne!(calculate(&other), EXPECTED);
        other = BASE;
        other.builtin_modules_hash = 1;
        assert_ne!(calculate(&other), EXPECTED);
        other = BASE;
        other.value_layout.size = 24;
        assert_ne!(calculate(&other), EXPECTED);
        other = BASE;
        other.value_layout.align = 16;
        assert_ne!(calculate(&other), EXPECTED);
        other = BASE;
        other.value_layout.fields = &[("tag", 0), ("payload", 4)];
        assert_ne!(calculate(&other), EXPECTED);
        other = BASE;
        other.value_layout.fields = &[("kind", 0), ("payload", 8)];
        assert_ne!(calculate(&other), EXPECTED);
        other = BASE;
        other.frame_layout.fields = &[("slots", 0), ("consts", 8), ("stack", 24)];
        assert_ne!(calculate(&other), EXPECTED);
        other = BASE;
        other.helpers = &[("op_add", "(Value)->Value")];
        assert_ne!(calculate(&other), EXPECTED);
        other = BASE;
        other.statics = &["null"];
        assert_ne!(calculate(&other), EXPECTED);
        other = BASE;
        other.data_version = 2;
        assert_ne!(calculate(&other), EXPECTED);
        other = BASE;
        other.code_version = 2;
        assert_ne!(calculate(&other), EXPECTED);
    }

    #[test]
    fn table_boundaries_and_order_are_significant() {
        let mut other = BASE;
        other.helpers = &[("op", "_add(Value,Value)->Value")];
        assert_ne!(calculate(&other), calculate(&BASE));
        other.helpers = &[("op_add", "(Value,Value)->Value"), ("x", "()")];
        assert_ne!(calculate(&other), calculate(&BASE));

        other.helpers = &[("a", "()"), ("b", "()")];
        let ordered = calculate(&other);
        other.helpers = &[("b", "()"), ("a", "()")];
        assert_ne!(calculate(&other), ordered);
        other = BASE;
        other.statics = &["a", "b"];
        let ordered = calculate(&other);
        other.statics = &["b", "a"];
        assert_ne!(calculate(&other), ordered);
    }
}
