//! Language-neutral AOT container. Format 6 retains the existing JS wire encoding;
//! language-specific manifests and codecs are interpreted by the engine.

pub const MAGIC: &[u8; 8] = b"LUMENAOT";
pub const FORMAT_VERSION: u32 = 6;
pub const HEADER_LEN: usize = 48;
pub const SECTION_ENTRY_LEN: usize = 24;

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
        let count = u32_at(20) as usize;
        let table_end = count
            .checked_mul(SECTION_ENTRY_LEN)
            .and_then(|n| HEADER_LEN.checked_add(n))
            .filter(|&end| end <= bytes.len())
            .ok_or("truncated section table")?;
        let mut sections = Vec::with_capacity(count);
        let mut ranges = Vec::with_capacity(count);
        for i in 0..count {
            let at = HEADER_LEN + i * SECTION_ENTRY_LEN;
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

/// Deterministic section-table encoding, shared by host producers.
pub fn encode(header: Header, sections: &[Section<'_>]) -> Result<Vec<u8>, &'static str> {
    let count = u32::try_from(sections.len()).map_err(|_| "too many sections")?;
    let table_end = sections
        .len()
        .checked_mul(SECTION_ENTRY_LEN)
        .and_then(|n| HEADER_LEN.checked_add(n))
        .ok_or("container too large")?;
    let total = sections
        .iter()
        .try_fold(table_end, |n, s| n.checked_add(s.data.len()))
        .ok_or("container too large")?;
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&header.codec_version.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&header.layout_fp.to_le_bytes());
    out.extend_from_slice(&header.lumen_version);
    let mut offset = table_end as u64;
    for s in sections {
        out.extend_from_slice(&s.kind.to_le_bytes());
        out.extend_from_slice(&s.flags.to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        out.extend_from_slice(&(s.data.len() as u64).to_le_bytes());
        offset += s.data.len() as u64;
    }
    for s in sections {
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
}
