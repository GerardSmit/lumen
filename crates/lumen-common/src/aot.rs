//! Language-neutral AOT containers. Format 6 retains the JS AST/bytecode wire
//! encoding; format 7 is the language-tagged native-only envelope.

pub const MAGIC: &[u8; 8] = b"LUMENAOT";
pub const FORMAT_VERSION: u32 = 6;
pub const HEADER_LEN: usize = 48;
pub const SECTION_ENTRY_LEN: usize = 24;
pub const NATIVE_FORMAT_VERSION: u32 = 7;
/// Increment when the linked code/GOT addressing contract changes.
pub const NATIVE_CODE_VERSION: u32 = 2;
pub const NATIVE_HEADER_LEN: usize = 64;
pub const SEC_NATIVE_CODE: u32 = 4;
pub const SEC_NATIVE_UNWIND: u32 = 11;
pub const SEC_NATIVE_LINES: u32 = 7;
pub const SEC_NATIVE_DATA: u32 = 9;
pub const SEC_NATIVE_GOT_RELOCS: u32 = 10;
pub const SEC_ASSETS: u32 = 12;

pub mod assets;
pub mod builtin_catalog;
pub mod fingerprint;
pub mod got;
pub mod install;
pub mod native_data;
pub mod native_lines;
pub mod native_unwind;
#[cfg(feature = "hash")]
pub mod sidecar;
#[cfg(feature = "native-signing")]
pub mod signature;

/// A stable, host-independent name inside an app's source tree.
pub fn normalized_source_path(name: &str) -> bool {
    !name.is_empty()
        && !name.contains('\\')
        && !name.contains('\0')
        && !name
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        && !name
            .split('/')
            .next()
            .is_some_and(|part| part.contains(':'))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Language {
    JavaScript = 1,
    Python = 2,
}

/// Read an unsigned LEB128 field, rejecting overflow before shifting.
pub fn read_varint(bytes: &[u8], pos: &mut usize) -> Result<u64, &'static str> {
    let mut value = 0;
    for shift in (0..70).step_by(7) {
        let byte = *bytes.get(*pos).ok_or("truncated varint")?;
        *pos += 1;
        if shift == 63 && byte > 1 {
            return Err("varint overflow");
        }
        value |= ((byte & 127) as u64) << shift;
        if byte & 128 == 0 {
            if shift != 0 && byte == 0 {
                return Err("non-canonical varint");
            }
            return Ok(value);
        }
    }
    Err("varint overflow")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub codec_version: u32,
    pub layout_fp: u64,
    pub lumen_version: [u8; 16],
}

pub fn version_bytes(version: &str) -> [u8; 16] {
    let mut bytes = [0; 16];
    let len = version.len().min(bytes.len());
    bytes[..len].copy_from_slice(&version.as_bytes()[..len]);
    bytes
}

#[derive(Clone, Copy, Debug)]
pub struct Section<'a> {
    pub kind: u32,
    pub flags: u32,
    pub data: &'a [u8],
}

#[derive(Debug)]
pub struct Container<'a> {
    pub header: Header,
    pub sections: Vec<Section<'a>>,
}

impl<'a> Container<'a> {
    /// Validate all ranges before handing borrowed payloads to a language decoder.
    /// Unknown section kinds are retained for forward-compatible consumers.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, &'static str> {
        if bytes.len() < HEADER_LEN || &bytes[..8] != MAGIC {
            return Err("not a lumen precompiled blob");
        }
        let u32_at = |at| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
        let u64_at = |at| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
        if u32_at(8) != FORMAT_VERSION {
            return Err("container format version mismatch (rebuild with this lumen)");
        }
        if u32_at(12) != 0 {
            return Err("unsupported container flags");
        }
        let sections = parse_sections(bytes, HEADER_LEN)?;
        Ok(Self {
            header: Header {
                codec_version: u32_at(16),
                layout_fp: u64_at(24),
                lumen_version: bytes[32..48].try_into().unwrap(),
            },
            sections,
        })
    }
}

/// Format 7 carries only native code and runtime data. It is separate from the
/// format-6 AST/bytecode decoder so a native-only runtime cannot accept either.
#[derive(Debug)]
pub struct NativeContainer<'a> {
    pub language: Language,
    pub data_version: u32,
    pub native_fp: u64,
    pub lumen_version: [u8; 16],
    pub functions: Vec<native_data::FunctionEntry>,
    pub required_imports: Vec<native_data::Import<'a>>,
    pub got_relocs: Vec<got::Reloc>,
    pub sections: Vec<Section<'a>>,
    code_offset: usize,
    blob_len: usize,
}

impl<'a> NativeContainer<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self, &'static str> {
        if bytes.len() < NATIVE_HEADER_LEN || &bytes[..8] != MAGIC {
            return Err("not a lumen native blob");
        }
        let u32_at = |at| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
        if u32_at(8) != NATIVE_FORMAT_VERSION {
            return Err("native container format version mismatch");
        }
        if u32_at(12) != 0 || u32_at(52) != 2 || bytes[56..64] != [0; 8] {
            return Err("unsupported native container flags or tier");
        }
        let language = match u32_at(48) {
            1 => Language::JavaScript,
            2 => Language::Python,
            _ => return Err("unknown native language"),
        };
        let native_fp = u64::from_le_bytes(bytes[24..32].try_into().unwrap());
        if native_fp == 0 {
            return Err("missing native fingerprint");
        }
        let sections = parse_sections(bytes, NATIVE_HEADER_LEN)?;
        for section in &sections {
            if section.flags != 0
                || !matches!(
                    section.kind,
                    SEC_NATIVE_CODE
                        | SEC_NATIVE_DATA
                        | SEC_NATIVE_GOT_RELOCS
                        | SEC_NATIVE_LINES
                        | SEC_NATIVE_UNWIND
                        | SEC_ASSETS
                )
            {
                return Err("unsupported native section");
            }
        }
        for kind in [SEC_NATIVE_CODE, SEC_NATIVE_DATA, SEC_NATIVE_GOT_RELOCS] {
            if sections.iter().filter(|s| s.kind == kind).count() != 1 {
                return Err("expected exactly one native code, data and GOT section");
            }
        }
        assets::validate_sections(&sections)?;
        if sections
            .iter()
            .filter(|s| s.kind == SEC_NATIVE_LINES)
            .count()
            > 1
        {
            return Err("duplicate native line tables");
        }
        if sections
            .iter()
            .any(|s| matches!(s.kind, SEC_NATIVE_CODE | SEC_NATIVE_DATA) && s.data.is_empty())
        {
            return Err("empty native code or data section");
        }
        if u32_at(16) != native_data::VERSION {
            return Err("native data version mismatch");
        }
        let code = sections
            .iter()
            .find(|s| s.kind == SEC_NATIVE_CODE)
            .unwrap()
            .data;
        let code_offset = (code.as_ptr() as usize)
            .checked_sub(bytes.as_ptr() as usize)
            .ok_or("native code offset out of bounds")?;
        let data = sections
            .iter()
            .find(|s| s.kind == SEC_NATIVE_DATA)
            .unwrap()
            .data;
        let native_data = native_data::NativeData::parse(data, code.len())?;
        let functions = native_data.functions;
        if let Some(lines) = sections.iter().find(|s| s.kind == SEC_NATIVE_LINES) {
            native_lines::decode(lines.data, &functions)?;
        }
        if sections
            .iter()
            .filter(|s| s.kind == SEC_NATIVE_UNWIND)
            .count()
            > 1
        {
            return Err("duplicate native unwind tables");
        }
        if let Some(unwind) = sections.iter().find(|s| s.kind == SEC_NATIVE_UNWIND) {
            native_unwind::decode(unwind.data, &functions)?;
        }
        let required_imports = native_data.imports;
        let relocs = got::decode(
            sections
                .iter()
                .find(|s| s.kind == SEC_NATIVE_GOT_RELOCS)
                .unwrap()
                .data,
        )?;
        if relocs
            .iter()
            .any(|r| r.kind == got::Kind::Function && r.index as usize >= functions.len())
        {
            return Err("native function GOT index out of range");
        }
        if relocs.iter().any(|r| {
            r.kind == got::Kind::NativeImport && r.index as usize >= required_imports.len()
        }) {
            return Err("native import GOT index out of range");
        }
        if relocs.iter().any(|r| {
            r.kind == got::Kind::NativeImport && required_imports[r.index as usize].name.is_empty()
        }) {
            return Err("module-only import cannot occupy a GOT slot");
        }
        Ok(Self {
            language,
            data_version: u32_at(16),
            native_fp,
            lumen_version: bytes[32..48].try_into().unwrap(),
            functions,
            required_imports,
            got_relocs: relocs,
            sections,
            code_offset,
            blob_len: bytes.len(),
        })
    }

    pub fn supported_by(&self, target: &crate::target::TargetSpec) -> Result<(), &'static str> {
        target.validate()?;
        if target.native_fp == 0
            || self.native_fp != target.native_fp
            || self.lumen_version != target.lumen_version
        {
            return Err("native version or fingerprint mismatch");
        }
        if target.arch == crate::target::Arch::Aarch64
            && self.functions.iter().any(|entry| entry.len % 4 != 0)
        {
            return Err("AArch64 native function length is not instruction aligned");
        }
        Ok(())
    }

    pub fn code_offset(&self) -> usize {
        self.code_offset
    }

    /// Exact contiguous code/GOT mapping size after page rounding. Validate
    /// placement before allocating pages or committing an installed slot.
    pub fn mapping_len(&self, target: &crate::target::TargetSpec) -> Result<usize, &'static str> {
        self.supported_by(target)?;
        let page = target.page_size as usize;
        let code_len = self
            .sections
            .iter()
            .find(|section| section.kind == SEC_NATIVE_CODE)
            .ok_or("missing native code section")?
            .data
            .len();
        if code_len % page != 0 {
            return Err("native code is not page aligned for RX/GOT separation");
        }
        let got_len = self
            .got_relocs
            .len()
            .checked_mul(8)
            .ok_or("native GOT too large")?;
        let got_pages = got_len
            .max(1)
            .checked_add(page - 1)
            .map(|len| len & !(page - 1))
            .ok_or("native GOT too large")?;
        if let crate::target::Placement::ExecuteInPlace { len, .. } = target.code_placement {
            if self.code_offset % page != 0 {
                return Err("native code file offset is not page aligned for XIP");
            }
            if u64::try_from(self.blob_len).map_err(|_| "native blob too large")? > len {
                return Err("native blob exceeds execute-in-place storage range");
            }
        }
        code_len
            .checked_add(got_pages)
            .ok_or("native image too large")
    }
}

fn parse_sections(bytes: &[u8], header_len: usize) -> Result<Vec<Section<'_>>, &'static str> {
    let u32_at = |at| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
    let u64_at = |at| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
    let count = u32_at(20) as usize;
    let table_end = count
        .checked_mul(SECTION_ENTRY_LEN)
        .and_then(|n| header_len.checked_add(n))
        .filter(|&end| end <= bytes.len())
        .ok_or("truncated section table")?;
    let mut sections = Vec::with_capacity(count);
    let mut ranges = Vec::with_capacity(count);
    for i in 0..count {
        let at = header_len + i * SECTION_ENTRY_LEN;
        let off = usize::try_from(u64_at(at + 8)).map_err(|_| "section out of bounds")?;
        let len = usize::try_from(u64_at(at + 16)).map_err(|_| "section out of bounds")?;
        let end = off
            .checked_add(len)
            .filter(|&end| end <= bytes.len())
            .ok_or("section out of bounds")?;
        if off < table_end {
            return Err("section overlaps header or table");
        }
        if len != 0 {
            ranges.push((off, end));
        }
        sections.push(Section {
            kind: u32_at(at),
            flags: u32_at(at + 4),
            data: &bytes[off..end],
        });
    }
    ranges.sort_unstable();
    if ranges.windows(2).any(|w| w[0].1 > w[1].0) {
        return Err("overlapping sections");
    }
    Ok(sections)
}

/// Deterministic section-table encoding, shared by host producers.
pub fn encode(header: Header, sections: &[Section<'_>]) -> Result<Vec<u8>, &'static str> {
    let mut out = Vec::with_capacity(HEADER_LEN);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&header.codec_version.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&header.layout_fp.to_le_bytes());
    out.extend_from_slice(&header.lumen_version);
    encode_sections(out, sections)
}

/// Encode a native-only blob. The caller supplies complete code, data and GOT
/// sections; format validation runs before the bytes are returned.
pub fn encode_native(
    language: Language,
    native_fp: u64,
    lumen_version: [u8; 16],
    sections: &[Section<'_>],
) -> Result<Vec<u8>, &'static str> {
    encode_native_aligned(language, native_fp, lumen_version, sections, 1)
}

/// For execute-in-place storage, align the code section's file offset to a
/// target page. The code bytes themselves remain page-sized and unmodified.
pub fn encode_native_aligned(
    language: Language,
    native_fp: u64,
    lumen_version: [u8; 16],
    sections: &[Section<'_>],
    code_alignment: usize,
) -> Result<Vec<u8>, &'static str> {
    if code_alignment == 0 || !code_alignment.is_power_of_two() {
        return Err("invalid native code alignment");
    }
    let mut out = Vec::with_capacity(NATIVE_HEADER_LEN);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&NATIVE_FORMAT_VERSION.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&native_data::VERSION.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&native_fp.to_le_bytes());
    out.extend_from_slice(&lumen_version);
    out.extend_from_slice(&(language as u32).to_le_bytes());
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&0u64.to_le_bytes());
    let out = encode_sections_aligned(out, sections, code_alignment)?;
    NativeContainer::parse(&out)?;
    Ok(out)
}

fn encode_sections(out: Vec<u8>, sections: &[Section<'_>]) -> Result<Vec<u8>, &'static str> {
    encode_sections_aligned(out, sections, 1)
}

fn encode_sections_aligned(
    mut out: Vec<u8>,
    sections: &[Section<'_>],
    code_alignment: usize,
) -> Result<Vec<u8>, &'static str> {
    let count = u32::try_from(sections.len()).map_err(|_| "too many sections")?;
    let table_end = sections
        .len()
        .checked_mul(SECTION_ENTRY_LEN)
        .and_then(|n| out.len().checked_add(n))
        .ok_or("container too large")?;
    let mut offsets = Vec::with_capacity(sections.len());
    let mut total = table_end;
    for section in sections {
        if section.kind == SEC_NATIVE_CODE && code_alignment != 1 {
            total = total
                .checked_add(code_alignment - 1)
                .map(|n| n & !(code_alignment - 1))
                .ok_or("container too large")?;
        }
        offsets.push(total);
        total = total
            .checked_add(section.data.len())
            .ok_or("container too large")?;
    }
    out[20..24].copy_from_slice(&count.to_le_bytes());
    out.reserve(total - out.len());
    for (s, &offset) in sections.iter().zip(&offsets) {
        out.extend_from_slice(&s.kind.to_le_bytes());
        out.extend_from_slice(&s.flags.to_le_bytes());
        out.extend_from_slice(
            &u64::try_from(offset)
                .map_err(|_| "container too large")?
                .to_le_bytes(),
        );
        out.extend_from_slice(
            &u64::try_from(s.data.len())
                .map_err(|_| "container too large")?
                .to_le_bytes(),
        );
    }
    for (s, &offset) in sections.iter().zip(&offsets) {
        out.resize(offset, 0);
        out.extend_from_slice(s.data);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blob() -> Vec<u8> {
        encode(
            Header {
                codec_version: 42,
                layout_fp: 123,
                lumen_version: version_bytes("0.1.0"),
            },
            &[
                Section {
                    kind: 1,
                    flags: 0,
                    data: b"manifest",
                },
                Section {
                    kind: 99,
                    flags: 7,
                    data: b"future",
                },
            ],
        )
        .unwrap()
    }

    #[test]
    fn varint_boundaries() {
        let mut pos = 0;
        assert_eq!(
            read_varint(
                &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 1],
                &mut pos
            ),
            Ok(u64::MAX)
        );
        for last in [2, 127, 128, 255] {
            let mut bytes = [255; 10];
            bytes[9] = last;
            assert!(read_varint(&bytes, &mut 0).is_err());
        }
        assert!(read_varint(&[128], &mut 0).is_err());
        assert!(read_varint(&[128, 0], &mut 0).is_err());
    }

    #[test]
    fn roundtrip_and_truncation() {
        let b = blob();
        let c = Container::parse(&b).unwrap();
        assert_eq!(c.header.codec_version, 42);
        assert_eq!(c.sections[1].data, b"future");
        assert_eq!(encode(c.header, &c.sections).unwrap(), b);
        for n in 0..b.len() {
            assert!(Container::parse(&b[..n]).is_err(), "accepted prefix {n}");
        }
    }

    #[test]
    fn rejects_aliased_overflowing_and_table_ranges() {
        for offset in [0, 48, 95, 96, u64::MAX] {
            let mut b = blob();
            b[80..88].copy_from_slice(&offset.to_le_bytes());
            assert!(Container::parse(&b).is_err(), "accepted offset {offset}");
        }
        let mut b = blob();
        b[20..24].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(Container::parse(&b).is_err());
        let mut b = blob();
        b[12] = 1;
        assert!(Container::parse(&b).is_err());
    }

    #[test]
    fn native_roundtrip_and_rejection() {
        let got_bytes = got::encode(&[got::Reloc {
            slot: 0,
            kind: got::Kind::Helper,
            index: 3,
        }])
        .unwrap();
        let data = native_data::encode(
            &[native_data::FunctionEntry { offset: 0, len: 4 }],
            b"data",
            4,
        )
        .unwrap();
        let sections = [
            Section {
                kind: SEC_NATIVE_CODE,
                flags: 0,
                data: b"code",
            },
            Section {
                kind: SEC_NATIVE_DATA,
                flags: 0,
                data: &data,
            },
            Section {
                kind: SEC_NATIVE_GOT_RELOCS,
                flags: 0,
                data: &got_bytes,
            },
        ];
        let mut empty_code = sections;
        empty_code[0].data = b"";
        assert!(encode_native(
            Language::JavaScript,
            42,
            version_bytes("0.1.0"),
            &empty_code
        )
        .is_err());
        let mut with_ast = sections.to_vec();
        with_ast.push(Section {
            kind: 2,
            flags: 0,
            data: b"ast",
        });
        assert!(
            encode_native(Language::JavaScript, 42, version_bytes("0.1.0"), &with_ast).is_err()
        );
        let mut bad_got = sections;
        bad_got[2].data = b"\x01";
        assert!(encode_native(Language::JavaScript, 42, version_bytes("0.1.0"), &bad_got).is_err());
        for language in [Language::JavaScript, Language::Python] {
            let blob = encode_native(language, 42, version_bytes("0.1.0"), &sections).unwrap();
            let parsed = NativeContainer::parse(&blob).unwrap();
            assert_eq!(parsed.language, language);
            assert_eq!(parsed.native_fp, 42);
            assert_eq!(parsed.sections[0].data, b"code");
            let target = crate::target::TargetSpec {
                lumen_version: version_bytes("0.1.0"),
                bytecode_fp: 1,
                native_fp: 42,
                arch: crate::target::Arch::Aarch64,
                abi: crate::target::Abi::Aapcs64,
                pointer_width: 64,
                page_size: 4096,
                features: 0,
                builtin_modules_hash: 0,
                profile: crate::target::Profile::Full,
                code_placement: crate::target::Placement::Ram,
            };
            assert!(parsed.supported_by(&target).is_ok());
            assert!(parsed
                .supported_by(&crate::target::TargetSpec {
                    bytecode_fp: 0,
                    profile: crate::target::Profile::Aot,
                    ..target
                })
                .is_ok());
            assert!(parsed
                .supported_by(&crate::target::TargetSpec {
                    native_fp: 0,
                    ..target
                })
                .is_err());
            assert!(Container::parse(&blob).is_err());
            for len in 0..blob.len() {
                assert!(
                    NativeContainer::parse(&blob[..len]).is_err(),
                    "accepted prefix {len}"
                );
            }
            for (at, value) in [(12, 1), (48, 3), (52, 1), (56, 1)] {
                let mut bad = blob.clone();
                bad[at] = value;
                assert!(NativeContainer::parse(&bad).is_err(), "accepted byte {at}");
            }
            let mut bad = blob.clone();
            bad[64..68].copy_from_slice(&2u32.to_le_bytes()); // AST in place of code
            assert!(NativeContainer::parse(&bad).is_err());
            let mut bad = blob.clone();
            bad[72..80].copy_from_slice(&0u64.to_le_bytes()); // payload aliases header
            assert!(NativeContainer::parse(&bad).is_err());
        }
    }
}
