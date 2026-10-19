//! Host linking of direct native calls through a writable GOT.
//!
//! Code and GOT have a fixed relative virtual layout: the GOT starts at the
//! next target page boundary after code. A loader must preserve that layout, including
//! when mapping code from execute-in-place storage. Only GOT slots are written
//! after the image is mapped.

use crate::x64::Compiled;
use lumen_common::aot::got::{Kind, Reloc};
use lumen_common::aot::{self, Language, Section};
use lumen_common::target::{Arch, Placement, TargetSpec};
use std::collections::BTreeMap;

/// Relocatable object format for a statically linked standalone runtime.
#[cfg(feature = "standalone-object")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectFormat {
    Elf,
    MachO,
    Coff,
}

/// Embed a validated AOT blob in a read-only object section. The standalone
/// runtime defines `main` and uses these two symbols to load the exact bytes.
#[cfg(feature = "standalone-object")]
pub fn standalone_object(
    blob: &[u8],
    target: &TargetSpec,
    format: ObjectFormat,
) -> Result<Vec<u8>, String> {
    use object::write::{Object, Symbol, SymbolSection};
    use object::{
        Architecture, BinaryFormat, Endianness, SectionKind, SymbolFlags, SymbolKind, SymbolScope,
    };

    target.validate().map_err(str::to_owned)?;
    if blob.is_empty() {
        return Err("standalone object needs a nonempty AOT blob".into());
    }
    let architecture = match target.arch {
        Arch::X86_64 => Architecture::X86_64,
        Arch::Aarch64 => Architecture::Aarch64,
        _ => return Err("standalone object needs a 64-bit native target".into()),
    };
    let (binary, segment, section) = match format {
        ObjectFormat::Elf => (BinaryFormat::Elf, b"".as_slice(), b".lumen_blob".as_slice()),
        ObjectFormat::MachO => (
            BinaryFormat::MachO,
            b"__LUMEN".as_slice(),
            b"__blob".as_slice(),
        ),
        ObjectFormat::Coff => (
            BinaryFormat::Coff,
            b"".as_slice(),
            b".rdata$lumen".as_slice(),
        ),
    };
    use lumen_common::target::Abi;
    let valid_abi = matches!(
        (format, target.arch, target.abi),
        (ObjectFormat::Elf, Arch::X86_64, Abi::SysV64)
            | (ObjectFormat::Elf, Arch::Aarch64, Abi::Aapcs64)
            | (ObjectFormat::MachO, Arch::X86_64, Abi::SysV64)
            | (ObjectFormat::MachO, Arch::Aarch64, Abi::Apple64)
            | (ObjectFormat::Coff, Arch::X86_64, Abi::Win64)
            | (ObjectFormat::Coff, Arch::Aarch64, Abi::Win64)
    );
    if !valid_abi {
        return Err("standalone object format differs from target ABI".into());
    }
    let mut file = Object::new(binary, architecture, Endianness::Little);
    let section = file.add_section(
        segment.to_vec(),
        section.to_vec(),
        SectionKind::ReadOnlyData,
    );
    file.append_section_data(section, blob, 16);
    for (name, offset) in [
        (b"lumen_aot_blob_start".as_slice(), 0),
        (b"lumen_aot_blob_end".as_slice(), blob.len() as u64),
        (b"lumen_aot_code_start".as_slice(), 0),
        (b"lumen_aot_code_end".as_slice(), 0),
        (b"lumen_aot_got_start".as_slice(), 0),
        (b"lumen_aot_got_end".as_slice(), 0),
    ] {
        file.add_symbol(Symbol {
            name: if format == ObjectFormat::MachO {
                [b"_".as_slice(), name].concat()
            } else {
                name.to_vec()
            },
            value: offset,
            size: 0,
            kind: SymbolKind::Data,
            scope: SymbolScope::Linkage,
            weak: false,
            section: SymbolSection::Section(section),
            flags: SymbolFlags::None,
        });
    }
    file.write().map_err(|error| error.to_string())
}

#[derive(Clone, Copy)]
enum FixupKind {
    X64Rip,
    ArmPage(u8),
}

#[derive(Clone, Copy)]
struct Fixup {
    offset: usize,
    symbol: (Kind, u32),
    encoding: FixupKind,
}

pub struct Function {
    target: TargetSpec,
    code: Vec<u8>,
    fixups: Vec<Fixup>,
    unwind: Vec<u8>,
    windows_unwind: Vec<u8>,
}

pub struct Image {
    target: TargetSpec,
    pub code: Vec<u8>,
    pub got_relocs: Vec<Reloc>,
    pub functions: Vec<aot::native_data::FunctionEntry>,
    pub unwind: Vec<aot::native_unwind::Record>,
    #[cfg(feature = "standalone-object")]
    static_fixups: Vec<(usize, u32, FixupKind)>,
}

impl Image {
    /// Emit statically linked text, GOT, unwind tables, and the authentic blob.
    /// GOT slots are resolved once by the linked runtime. Only helper slots are
    /// supported because realm-local addresses cannot live in process-static GOT.
    #[cfg(feature = "standalone-object")]
    pub fn standalone_link_object(
        &self,
        blob: &[u8],
        target: &TargetSpec,
        format: ObjectFormat,
    ) -> Result<Vec<u8>, String> {
        use object::write::{Object, Relocation, Symbol, SymbolSection};
        use object::{
            elf, macho, pe, Architecture, BinaryFormat, Endianness, RelocationEncoding,
            RelocationFlags, RelocationKind, SectionKind, SymbolFlags, SymbolKind, SymbolScope,
        };
        if *target != self.target || target.native_fp == 0 {
            return Err("standalone native image target mismatch".into());
        }
        let container = aot::NativeContainer::parse(blob).map_err(str::to_owned)?;
        container.mapping_len(target).map_err(str::to_owned)?;
        let encoded_code = container
            .sections
            .iter()
            .find(|section| section.kind == aot::SEC_NATIVE_CODE)
            .ok_or("standalone native code missing")?
            .data;
        if encoded_code != self.code
            || container.got_relocs != self.got_relocs
            || container.functions != self.functions
        {
            return Err("standalone object differs from encoded native image".into());
        }
        let encoded_unwind = container
            .sections
            .iter()
            .find(|section| section.kind == aot::SEC_NATIVE_UNWIND)
            .ok_or("standalone native unwind missing")?
            .data;
        let records =
            aot::native_unwind::decode(encoded_unwind, &self.functions).map_err(str::to_owned)?;
        if records.len() != self.unwind.len()
            || records
                .iter()
                .zip(&self.unwind)
                .any(|(a, b)| a.function != b.function || a.cfi != b.cfi || a.windows != b.windows)
        {
            return Err("standalone unwind differs from encoded native image".into());
        }
        if self
            .got_relocs
            .iter()
            .any(|reloc| reloc.kind != Kind::Helper)
        {
            return Err("standalone linked GOT requires process-stable helper symbols".into());
        }
        let architecture = match target.arch {
            Arch::X86_64 => Architecture::X86_64,
            Arch::Aarch64 => Architecture::Aarch64,
            _ => return Err("standalone native object needs a 64-bit target".into()),
        };
        let binary = match format {
            ObjectFormat::Elf => BinaryFormat::Elf,
            ObjectFormat::MachO => BinaryFormat::MachO,
            ObjectFormat::Coff => BinaryFormat::Coff,
        };
        let expected = matches!(
            (format, target.arch, target.abi),
            (
                ObjectFormat::Elf,
                Arch::X86_64,
                lumen_common::target::Abi::SysV64
            ) | (
                ObjectFormat::Elf,
                Arch::Aarch64,
                lumen_common::target::Abi::Aapcs64
            ) | (
                ObjectFormat::MachO,
                Arch::X86_64,
                lumen_common::target::Abi::SysV64
            ) | (
                ObjectFormat::MachO,
                Arch::Aarch64,
                lumen_common::target::Abi::Apple64
            ) | (
                ObjectFormat::Coff,
                Arch::X86_64,
                lumen_common::target::Abi::Win64
            ) | (
                ObjectFormat::Coff,
                Arch::Aarch64,
                lumen_common::target::Abi::Win64
            )
        );
        if !expected {
            return Err("standalone object format differs from target ABI".into());
        }
        let mut object = Object::new(binary, architecture, Endianness::Little);
        let (blob_segment, blob_name, code_segment, code_name, got_segment, got_name) = match format
        {
            ObjectFormat::Elf => (
                b"".as_slice(),
                b".lumen_blob".as_slice(),
                b"".as_slice(),
                b".text.lumen".as_slice(),
                b"".as_slice(),
                b".data.lumen_got".as_slice(),
            ),
            ObjectFormat::MachO => (
                b"__LUMEN".as_slice(),
                b"__blob".as_slice(),
                b"__TEXT".as_slice(),
                b"__text".as_slice(),
                b"__DATA".as_slice(),
                b"__got".as_slice(),
            ),
            ObjectFormat::Coff => (
                b"".as_slice(),
                b".rdata$lumen".as_slice(),
                b"".as_slice(),
                b".text$lumen".as_slice(),
                b"".as_slice(),
                b".data$lumen".as_slice(),
            ),
        };
        let blob_section = object.add_section(
            blob_segment.to_vec(),
            blob_name.to_vec(),
            SectionKind::ReadOnlyData,
        );
        object.append_section_data(blob_section, blob, 16);
        let code_section =
            object.add_section(code_segment.to_vec(), code_name.to_vec(), SectionKind::Text);
        let mut code = self.code.clone();
        for &(offset, _, kind) in &self.static_fixups {
            match kind {
                FixupKind::X64Rip => code[offset..offset + 4].fill(0),
                FixupKind::ArmPage(dst) => {
                    code[offset..offset + 4].copy_from_slice(&0x9000_0010u32.to_le_bytes());
                    code[offset + 4..offset + 8]
                        .copy_from_slice(&(0xf940_0200u32 | dst as u32).to_le_bytes());
                }
            }
        }
        object.append_section_data(code_section, &code, target.page_size as u64);
        let got_section =
            object.add_section(got_segment.to_vec(), got_name.to_vec(), SectionKind::Data);
        let got_len = self
            .got_relocs
            .len()
            .checked_mul(8)
            .ok_or("standalone GOT too large")?;
        object.append_section_data(got_section, &vec![0; got_len.max(8)], 8);
        let add_symbol = |object: &mut Object<'_>, section, name: &[u8], value, kind| {
            object.add_symbol(Symbol {
                name: if format == ObjectFormat::MachO && !name.is_empty() {
                    [b"_".as_slice(), name].concat()
                } else {
                    name.to_vec()
                },
                value,
                size: 0,
                kind,
                scope: if name.is_empty() {
                    SymbolScope::Compilation
                } else {
                    SymbolScope::Linkage
                },
                weak: false,
                section: SymbolSection::Section(section),
                flags: SymbolFlags::None,
            })
        };
        for (section, start, end, len, kind) in [
            (
                blob_section,
                b"lumen_aot_blob_start".as_slice(),
                b"lumen_aot_blob_end".as_slice(),
                blob.len(),
                SymbolKind::Data,
            ),
            (
                code_section,
                b"lumen_aot_code_start".as_slice(),
                b"lumen_aot_code_end".as_slice(),
                code.len(),
                SymbolKind::Text,
            ),
            (
                got_section,
                b"lumen_aot_got_start".as_slice(),
                b"lumen_aot_got_end".as_slice(),
                got_len,
                SymbolKind::Data,
            ),
        ] {
            add_symbol(&mut object, section, start, 0, kind);
            add_symbol(&mut object, section, end, len as u64, kind);
        }
        let got_symbols = (0..self.got_relocs.len())
            .map(|slot| {
                add_symbol(
                    &mut object,
                    got_section,
                    &[],
                    (slot * 8) as u64,
                    SymbolKind::Data,
                )
            })
            .collect::<Vec<_>>();
        for &(offset, slot, kind) in &self.static_fixups {
            let symbol = *got_symbols
                .get(slot as usize)
                .ok_or("standalone GOT slot missing")?;
            let mut add_reloc = |offset: usize, addend: i64, flags| {
                object
                    .add_relocation(
                        code_section,
                        Relocation {
                            offset: offset as u64,
                            symbol,
                            addend,
                            flags,
                        },
                    )
                    .map_err(|error| error.to_string())
            };
            match kind {
                FixupKind::X64Rip => add_reloc(
                    offset,
                    -4,
                    RelocationFlags::Generic {
                        kind: RelocationKind::Relative,
                        encoding: RelocationEncoding::X86RipRelative,
                        size: 32,
                    },
                )?,
                FixupKind::ArmPage(_) => {
                    let (page, lo) = match format {
                        ObjectFormat::Elf => (
                            RelocationFlags::Elf {
                                r_type: elf::R_AARCH64_ADR_PREL_PG_HI21,
                            },
                            RelocationFlags::Elf {
                                r_type: elf::R_AARCH64_LDST64_ABS_LO12_NC,
                            },
                        ),
                        ObjectFormat::MachO => (
                            RelocationFlags::MachO {
                                r_type: macho::ARM64_RELOC_PAGE21,
                                r_pcrel: true,
                                r_length: 2,
                            },
                            RelocationFlags::MachO {
                                r_type: macho::ARM64_RELOC_PAGEOFF12,
                                r_pcrel: false,
                                r_length: 2,
                            },
                        ),
                        ObjectFormat::Coff => (
                            RelocationFlags::Coff {
                                typ: pe::IMAGE_REL_ARM64_PAGEBASE_REL21,
                            },
                            RelocationFlags::Coff {
                                typ: pe::IMAGE_REL_ARM64_PAGEOFFSET_12L,
                            },
                        ),
                    };
                    add_reloc(offset, 0, page)?;
                    add_reloc(offset + 4, 0, lo)?;
                }
            }
        }
        if format != ObjectFormat::Coff {
            let (frame, relocations) =
                aot::native_unwind::eh_frame_pic(target.arch, &self.functions, &self.unwind)
                    .map_err(str::to_owned)?;
            let frame_name = if format == ObjectFormat::MachO {
                b"__eh_frame".as_slice()
            } else {
                b".eh_frame".as_slice()
            };
            let frame_section = object.add_section(
                if format == ObjectFormat::MachO {
                    b"__TEXT".to_vec()
                } else {
                    vec![]
                },
                frame_name.to_vec(),
                SectionKind::ReadOnlyData,
            );
            object.append_section_data(frame_section, &frame, 8);
            // object 0.36 emits Mach-O relocations in insertion order when
            // offsets descend. A SUBTRACTOR must immediately precede its
            // UNSIGNED partner at the same location.
            for (offset, index) in relocations.into_iter().rev() {
                let function = self
                    .functions
                    .get(index as usize)
                    .ok_or("unwind function missing")?;
                let symbol = add_symbol(
                    &mut object,
                    code_section,
                    &[],
                    function.offset as u64,
                    SymbolKind::Text,
                );
                if format == ObjectFormat::MachO {
                    let field = add_symbol(
                        &mut object,
                        frame_section,
                        &[],
                        offset as u64,
                        SymbolKind::Data,
                    );
                    let (subtractor, unsigned) = if target.arch == Arch::X86_64 {
                        (macho::X86_64_RELOC_SUBTRACTOR, macho::X86_64_RELOC_UNSIGNED)
                    } else {
                        (macho::ARM64_RELOC_SUBTRACTOR, macho::ARM64_RELOC_UNSIGNED)
                    };
                    for (symbol, r_type) in [(field, subtractor), (symbol, unsigned)] {
                        object
                            .add_relocation(
                                frame_section,
                                Relocation {
                                    offset: offset as u64,
                                    symbol,
                                    addend: 0,
                                    flags: RelocationFlags::MachO {
                                        r_type,
                                        r_pcrel: false,
                                        r_length: 3,
                                    },
                                },
                            )
                            .map_err(|error| error.to_string())?;
                    }
                } else {
                    let r_type = if target.arch == Arch::X86_64 {
                        elf::R_X86_64_PC64
                    } else {
                        elf::R_AARCH64_PREL64
                    };
                    object
                        .add_relocation(
                            frame_section,
                            Relocation {
                                offset: offset as u64,
                                symbol,
                                addend: 0,
                                flags: RelocationFlags::Elf { r_type },
                            },
                        )
                        .map_err(|error| error.to_string())?;
                }
            }
        } else {
            let xdata =
                object.add_section(vec![], b".xdata$lumen".to_vec(), SectionKind::ReadOnlyData);
            let pdata =
                object.add_section(vec![], b".pdata$lumen".to_vec(), SectionKind::ReadOnlyData);
            let mut covered = std::collections::BTreeSet::new();
            for (index, function) in self.functions.iter().enumerate() {
                if !covered.insert(function.offset) {
                    continue;
                }
                let recipe = &self.unwind[index].windows;
                if recipe.is_empty() {
                    return Err("Windows native function has no unwind recipe".into());
                }
                aot::native_unwind::validate_windows(target.arch, recipe, function.len)
                    .map_err(str::to_owned)?;
                let xoff = object.append_section_data(xdata, recipe, 4);
                let poff = object.append_section_data(
                    pdata,
                    &vec![0; if target.arch == Arch::Aarch64 { 8 } else { 12 }],
                    4,
                );
                let begin = add_symbol(
                    &mut object,
                    code_section,
                    &[],
                    function.offset as u64,
                    SymbolKind::Text,
                );
                let unwind = add_symbol(&mut object, xdata, &[], xoff, SymbolKind::Data);
                let mut fields = vec![(poff, begin)];
                if target.arch == Arch::X86_64 {
                    let end = add_symbol(
                        &mut object,
                        code_section,
                        &[],
                        function
                            .offset
                            .checked_add(function.len)
                            .ok_or("unwind range overflow")? as u64,
                        SymbolKind::Text,
                    );
                    fields.push((poff + 4, end));
                    fields.push((poff + 8, unwind));
                } else {
                    fields.push((poff + 4, unwind));
                }
                for (offset, symbol) in fields {
                    object
                        .add_relocation(
                            pdata,
                            Relocation {
                                offset,
                                symbol,
                                addend: 0,
                                flags: RelocationFlags::Generic {
                                    kind: RelocationKind::ImageOffset,
                                    encoding: RelocationEncoding::Generic,
                                    size: 32,
                                },
                            },
                        )
                        .map_err(|error| error.to_string())?;
                }
            }
        }
        object.write().map_err(|error| error.to_string())
    }

    /// Add the language-specific payload after the shared function table.
    pub fn encode_data(&self, payload: &[u8]) -> Result<Vec<u8>, &'static str> {
        self.encode_data_with_imports(&[], payload)
    }

    pub fn encode_data_with_imports(
        &self,
        imports: &[aot::native_data::Import<'_>],
        payload: &[u8],
    ) -> Result<Vec<u8>, &'static str> {
        aot::native_data::encode_with_imports(&self.functions, imports, payload, self.code.len())
    }

    /// Serialize code, shared/language data and GOT relocations in format 7.
    /// Optional `lines` must use `aot::native_lines::encode`.
    pub fn encode_native(
        &self,
        language: Language,
        target: &TargetSpec,
        payload: &[u8],
        lines: Option<&[u8]>,
    ) -> Result<Vec<u8>, String> {
        self.encode_native_with_imports(language, target, &[], payload, lines)
    }

    pub fn encode_native_with_imports(
        &self,
        language: Language,
        target: &TargetSpec,
        imports: &[aot::native_data::Import<'_>],
        payload: &[u8],
        lines: Option<&[u8]>,
    ) -> Result<Vec<u8>, String> {
        target.validate().map_err(str::to_owned)?;
        if *target != self.target || target.native_fp == 0 {
            return Err("native image target mismatch".into());
        }
        let data = self
            .encode_data_with_imports(imports, payload)
            .map_err(str::to_owned)?;
        let got = aot::got::encode(&self.got_relocs).map_err(str::to_owned)?;
        let unwind = aot::native_unwind::encode(&self.unwind).map_err(str::to_owned)?;
        let mut sections = vec![
            Section {
                kind: aot::SEC_NATIVE_CODE,
                flags: 0,
                data: &self.code,
            },
            Section {
                kind: aot::SEC_NATIVE_DATA,
                flags: 0,
                data: &data,
            },
            Section {
                kind: aot::SEC_NATIVE_GOT_RELOCS,
                flags: 0,
                data: &got,
            },
            Section {
                kind: aot::SEC_NATIVE_UNWIND,
                flags: 0,
                data: &unwind,
            },
        ];
        if let Some(lines) = lines {
            sections.push(Section {
                kind: aot::SEC_NATIVE_LINES,
                flags: 0,
                data: lines,
            });
        }
        let alignment = if matches!(target.code_placement, Placement::ExecuteInPlace { .. }) {
            target.page_size as usize
        } else {
            1
        };
        let blob = aot::encode_native_aligned(
            language,
            target.native_fp,
            target.lumen_version,
            &sections,
            alignment,
        )
        .map_err(str::to_owned)?;
        aot::NativeContainer::parse(&blob)
            .map_err(str::to_owned)?
            .mapping_len(target)
            .map_err(str::to_owned)?;
        Ok(blob)
    }

    /// Embed validated source locations for device-side diagnostics.
    pub fn encode_native_with_locations(
        &self,
        language: Language,
        target: &TargetSpec,
        imports: &[aot::native_data::Import<'_>],
        payload: &[u8],
        files: &[&str],
        locations: &[aot::native_lines::Location],
    ) -> Result<Vec<u8>, String> {
        let lines =
            aot::native_lines::encode(files, locations, &self.functions).map_err(str::to_owned)?;
        self.encode_native_with_imports(language, target, imports, payload, Some(&lines))
    }

    /// Emit a blob without on-device line tables and a matching host-side map.
    /// The map records locations relative to each function entry and is bound
    /// to the exact final blob bytes, including the target and payload.
    pub fn encode_native_stripped(
        &self,
        language: Language,
        target: &TargetSpec,
        imports: &[aot::native_data::Import<'_>],
        payload: &[u8],
        files: Vec<String>,
        locations: Vec<aot::sidecar::Location>,
    ) -> Result<(Vec<u8>, Vec<u8>), String> {
        if files.iter().any(|file| !aot::normalized_source_path(file)) {
            return Err("native line-map paths must be relative, normalized slash paths".into());
        }
        let blob = self.encode_native_with_imports(language, target, imports, payload, None)?;
        let map = aot::sidecar::Sidecar::new(&blob, files, locations)
            .map_err(str::to_owned)?
            .encode()
            .map_err(str::to_owned)?;
        Ok((blob, map))
    }
}

/// Convert backend direct calls and symbolic-address loads to GOT references.
/// The front end classifies each imported id as a helper, native import, blob
/// function, or other GOT symbol. JIT output is left unchanged.
pub fn function(
    target: &TargetSpec,
    compiled: Compiled,
    symbol: impl Fn(u32) -> Option<(Kind, u32)>,
) -> Result<Function, String> {
    target.validate().map_err(str::to_owned)?;
    if target.native_fp == 0 {
        return Err("target does not advertise a native ABI".into());
    }
    let mut code = compiled.code;
    let mut fixups = Vec::new();
    match target.arch {
        Arch::X86_64 => {
            if !compiled.direct_calls.is_empty() {
                return Err("x64: unexpected AArch64 call sites".into());
            }
            for reloc in compiled.relocs {
                let at = reloc.offset;
                if at < 2
                    || at.checked_add(11).is_none_or(|end| end > code.len())
                    || code[at - 2..at] != [0x49, 0xbb]
                    || code[at + 8..at + 11] != [0x41, 0xff, 0xd3]
                {
                    return Err("x64: unsupported direct-call relocation".into());
                }
                // mov r11, [rip + disp32]; nop*3; call r11. Keep the original
                // length so branch displacements and constant-pool offsets hold.
                code[at - 2..at + 1].copy_from_slice(&[0x4c, 0x8b, 0x1d]);
                code[at + 1..at + 5].fill(0);
                code[at + 5..at + 8].fill(0x90);
                fixups.push(Fixup {
                    offset: at + 1,
                    symbol: symbol(reloc.func_id).ok_or("unresolved native call symbol")?,
                    encoding: FixupKind::X64Rip,
                });
            }
            for reloc in compiled.symbol_loads {
                let at = reloc.offset;
                if at < 3
                    || at.checked_add(4).is_none_or(|end| end > code.len())
                    || code[at - 2] != 0x8b
                    || code[at - 1] & 0xc7 != 0x05
                    || code[at - 3] & 0xfb != 0x48
                {
                    return Err("x64: unsupported symbolic-address load".into());
                }
                fixups.push(Fixup {
                    offset: at,
                    symbol: symbol(reloc.func_id).ok_or("unresolved native address symbol")?,
                    encoding: FixupKind::X64Rip,
                });
            }
        }
        Arch::Aarch64 => {
            let mut symbols = BTreeMap::new();
            let mut stubs = BTreeMap::new();
            for reloc in &compiled.relocs {
                if reloc
                    .offset
                    .checked_add(8)
                    .is_none_or(|end| end > code.len())
                {
                    return Err("aarch64: bad literal relocation".into());
                }
                symbols.insert(reloc.func_id, reloc.offset);
                code[reloc.offset..reloc.offset + 8].fill(0);
            }
            for call in compiled.direct_calls {
                if !symbols.contains_key(&call.func_id)
                    || call
                        .offset
                        .checked_add(8)
                        .is_none_or(|end| end > code.len())
                {
                    return Err("aarch64: missing direct-call literal".into());
                }
                let ldr =
                    u32::from_le_bytes(code[call.offset..call.offset + 4].try_into().unwrap());
                let blr =
                    u32::from_le_bytes(code[call.offset + 4..call.offset + 8].try_into().unwrap());
                let imm19 = ((ldr >> 5) & 0x7ffff) as i32;
                let displacement = (imm19 << 13) >> 11;
                let literal = call.offset as i64 + displacement as i64;
                if ldr & 0xff00_001f != 0x5800_0010
                    || blr != 0xd63f_0200
                    || literal != symbols[&call.func_id] as i64
                    || code.len() % 4 != 0
                {
                    return Err("aarch64: unsupported direct-call sequence".into());
                }
                let stub = if let Some(&stub) = stubs.get(&call.func_id) {
                    stub
                } else {
                    let stub = code.len();
                    code.extend_from_slice(&0x9000_0010u32.to_le_bytes()); // adrp x16, GOT page
                    code.extend_from_slice(&0xf940_0210u32.to_le_bytes()); // ldr x16, [x16, #lo12]
                    code.extend_from_slice(&0xd61f_0200u32.to_le_bytes()); // br x16
                    fixups.push(Fixup {
                        offset: stub,
                        symbol: symbol(call.func_id).ok_or("unresolved native call symbol")?,
                        encoding: FixupKind::ArmPage(16),
                    });
                    stubs.insert(call.func_id, stub);
                    stub
                };
                let branch = (stub as i64 - call.offset as i64) / 4;
                if !(-(1 << 25)..(1 << 25)).contains(&branch) {
                    return Err("aarch64: GOT call stub out of range".into());
                }
                // BL stub; NOP. The stub tail-branches to the callee so its
                // return address remains the instruction after BL.
                let bl = 0x9400_0000 | ((branch as u32) & 0x03ff_ffff);
                code[call.offset..call.offset + 4].copy_from_slice(&bl.to_le_bytes());
                code[call.offset + 4..call.offset + 8]
                    .copy_from_slice(&0xd503_201fu32.to_le_bytes());
            }
            for reloc in compiled.symbol_loads {
                let at = reloc.offset;
                if at.checked_add(8).is_none_or(|end| end > code.len())
                    || u32::from_le_bytes(code[at..at + 4].try_into().unwrap()) != 0x9000_0010
                {
                    return Err("aarch64: unsupported symbolic-address load".into());
                }
                let load = u32::from_le_bytes(code[at + 4..at + 8].try_into().unwrap());
                if load & 0xffff_ffe0 != 0xf940_0200 {
                    return Err("aarch64: unsupported symbolic-address load".into());
                }
                fixups.push(Fixup {
                    offset: at,
                    symbol: symbol(reloc.func_id).ok_or("unresolved native address symbol")?,
                    encoding: FixupKind::ArmPage((load & 31) as u8),
                });
            }
            // Old address literals are unreachable and remain zero. No code
            // relocation is retained for the device.
        }
        _ => return Err("native AOT backend unavailable for target".into()),
    }
    Ok(Function {
        target: *target,
        code,
        fixups,
        unwind: compiled.unwind,
        windows_unwind: compiled.windows_unwind,
    })
}

/// Link functions in order. The returned code length is page-aligned so the
/// loader can map GOT pages RW while keeping code pages RX.
pub fn link(functions: &[Function]) -> Result<Image, String> {
    if functions.is_empty() {
        return Err("native image has no functions".into());
    }
    let mut code = Vec::new();
    let mut entries = Vec::with_capacity(functions.len());
    let mut unwind = Vec::with_capacity(functions.len());
    let mut fixups = Vec::new();
    let mut folded = BTreeMap::new();
    let target = functions[0].target;
    for function in functions {
        if function.target != target {
            return Err("mixed targets in native image".into());
        }
        let len = u32::try_from(function.code.len()).map_err(|_| "native function too large")?;
        if len == 0 {
            return Err("empty native function".into());
        }
        unwind.push(aot::native_unwind::Record {
            function: u32::try_from(entries.len()).map_err(|_| "too many native functions")?,
            cfi: function.unwind.clone(),
            windows: function.windows_unwind.clone(),
        });
        let key = (
            function.code.clone(),
            function.unwind.clone(),
            function.windows_unwind.clone(),
            function
                .fixups
                .iter()
                .map(|fixup| {
                    (
                        fixup.offset,
                        fixup.symbol.0 as u32,
                        fixup.symbol.1,
                        match fixup.encoding {
                            FixupKind::X64Rip => 1u8,
                            FixupKind::ArmPage(dst) => dst + 2,
                        },
                    )
                })
                .collect::<Vec<_>>(),
        );
        if let Some(&entry) = folded.get(&key) {
            entries.push(aot::native_data::FunctionEntry { offset: entry, len });
            continue;
        }
        code.resize(align(code.len(), 16)?, 0);
        let base = code.len();
        let entry = u32::try_from(base).map_err(|_| "native code section too large")?;
        entries.push(aot::native_data::FunctionEntry { offset: entry, len });
        folded.insert(key, entry);
        code.extend_from_slice(&function.code);
        for fixup in &function.fixups {
            fixups.push(Fixup {
                offset: base
                    .checked_add(fixup.offset)
                    .ok_or("native code section too large")?,
                ..*fixup
            });
        }
    }
    code.resize(align(code.len(), target.page_size as usize)?, 0);
    let got_base = code.len();
    let mut slots = BTreeMap::new();
    let mut got_relocs = Vec::new();
    #[cfg(feature = "standalone-object")]
    let mut static_fixups = Vec::new();
    for fixup in fixups {
        if fixup.symbol.0 == Kind::Function && fixup.symbol.1 as usize >= functions.len() {
            return Err("native function call index out of range".into());
        }
        let key = (fixup.symbol.0 as u32, fixup.symbol.1);
        let slot = match slots.get(&key) {
            Some(&slot) => slot,
            None => {
                let slot = u32::try_from(got_relocs.len()).map_err(|_| "too many GOT entries")?;
                got_relocs.push(Reloc {
                    slot,
                    kind: fixup.symbol.0,
                    index: fixup.symbol.1,
                });
                slots.insert(key, slot);
                slot
            }
        };
        let slot_offset = (slot as usize)
            .checked_mul(8)
            .ok_or("GOT offset overflow")?;
        #[cfg(feature = "standalone-object")]
        static_fixups.push((fixup.offset, slot, fixup.encoding));
        let target = got_base
            .checked_add(slot_offset)
            .ok_or("GOT offset overflow")?;
        match fixup.encoding {
            FixupKind::X64Rip => {
                let next = fixup.offset.checked_add(4).ok_or("code offset overflow")?;
                let disp = i32::try_from(target as i64 - next as i64)
                    .map_err(|_| "x64: GOT out of RIP-relative range")?;
                code[fixup.offset..next].copy_from_slice(&disp.to_le_bytes());
            }
            FixupKind::ArmPage(dst) => {
                let page_delta = (target >> 12) as i64 - (fixup.offset >> 12) as i64;
                if !(-(1 << 20)..(1 << 20)).contains(&page_delta) {
                    return Err("aarch64: GOT out of ADRP range".into());
                }
                let imm = page_delta as u32;
                let adrp = 0x9000_0010 | ((imm & 3) << 29) | (((imm >> 2) & 0x7ffff) << 5);
                code[fixup.offset..fixup.offset + 4].copy_from_slice(&adrp.to_le_bytes());
                let ldr = 0xf940_0200 | dst as u32 | (((target & 4095) as u32 / 8) << 10);
                code[fixup.offset + 4..fixup.offset + 8].copy_from_slice(&ldr.to_le_bytes());
            }
        }
    }
    Ok(Image {
        target,
        code,
        got_relocs,
        functions: entries,
        unwind,
        #[cfg(feature = "standalone-object")]
        static_fixups,
    })
}

fn align(value: usize, n: usize) -> Result<usize, String> {
    value
        .checked_add(n - 1)
        .map(|v| v & !(n - 1))
        .ok_or_else(|| "native image too large".into())
}
