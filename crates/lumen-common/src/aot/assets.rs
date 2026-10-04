//! Deterministic, language-neutral application assets.

use super::{normalized_source_path, Section, SEC_ASSETS};

pub struct Archive<'a>(Vec<(&'a str, &'a [u8])>);

impl<'a> Archive<'a> {
    pub fn decode(bytes: &'a [u8]) -> Result<Self, &'static str> {
        let mut cursor = 0usize;
        fn take<'a>(
            bytes: &'a [u8],
            cursor: &mut usize,
            length: usize,
        ) -> Result<&'a [u8], &'static str> {
            let end = cursor.checked_add(length).ok_or("asset range overflow")?;
            let value = bytes.get(*cursor..end).ok_or("truncated asset archive")?;
            *cursor = end;
            Ok(value)
        }
        if take(bytes, &mut cursor, 8)? != b"LUMASSET" {
            return Err("invalid asset archive magic");
        }
        if take(bytes, &mut cursor, 4)? != 1u32.to_le_bytes() {
            return Err("unsupported asset archive version");
        }
        let count = u32::from_le_bytes(take(bytes, &mut cursor, 4)?.try_into().unwrap()) as usize;
        if count > bytes.len() / 13 {
            return Err("invalid asset count");
        }
        let mut entries: Vec<(&str, &[u8])> = Vec::with_capacity(count);
        for _ in 0..count {
            let length =
                u32::from_le_bytes(take(bytes, &mut cursor, 4)?.try_into().unwrap()) as usize;
            let name = std::str::from_utf8(take(bytes, &mut cursor, length)?)
                .map_err(|_| "invalid asset path UTF-8")?;
            if !normalized_source_path(name)
                || entries
                    .last()
                    .is_some_and(|(previous, _)| *previous >= name)
            {
                return Err("asset paths must be normalized, unique and sorted");
            }
            let length = u64::from_le_bytes(take(bytes, &mut cursor, 8)?.try_into().unwrap());
            let length = usize::try_from(length).map_err(|_| "asset length overflow")?;
            entries.push((name, take(bytes, &mut cursor, length)?));
        }
        if cursor != bytes.len() {
            return Err("trailing asset archive bytes");
        }
        Ok(Self(entries))
    }

    pub fn get(&self, path: &str) -> Option<&'a [u8]> {
        self.0
            .binary_search_by_key(&path, |(name, _)| *name)
            .ok()
            .map(|index| self.0[index].1)
    }

    pub fn entries(&self) -> &[(&'a str, &'a [u8])] {
        &self.0
    }
}

pub fn encode(entries: &[(&str, &[u8])]) -> Result<Vec<u8>, &'static str> {
    let mut entries = entries.to_vec();
    entries.sort_unstable_by_key(|(name, _)| *name);
    let mut out = b"LUMASSET".to_vec();
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(
        &u32::try_from(entries.len())
            .map_err(|_| "too many assets")?
            .to_le_bytes(),
    );
    for (name, data) in entries {
        if !normalized_source_path(name) {
            return Err("invalid asset path");
        }
        out.extend_from_slice(
            &u32::try_from(name.len())
                .map_err(|_| "asset path too long")?
                .to_le_bytes(),
        );
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&(data.len() as u64).to_le_bytes());
        out.extend_from_slice(data);
    }
    Archive::decode(&out)?;
    Ok(out)
}

pub(super) fn validate_sections(sections: &[Section<'_>]) -> Result<(), &'static str> {
    let mut assets = sections.iter().filter(|section| section.kind == SEC_ASSETS);
    if let Some(section) = assets.next() {
        if section.flags != 0 || assets.next().is_some() {
            return Err("invalid asset section");
        }
        Archive::decode(section.data)?;
    }
    Ok(())
}

pub fn from_blob(blob: &[u8]) -> Result<Option<Archive<'_>>, &'static str> {
    let sections = if blob.get(8..12) == Some(&super::NATIVE_FORMAT_VERSION.to_le_bytes()) {
        super::NativeContainer::parse(blob)?.sections
    } else {
        super::Container::parse(blob)?.sections
    };
    validate_sections(&sections)?;
    sections
        .iter()
        .find(|section| section.kind == SEC_ASSETS)
        .map(|section| Archive::decode(section.data))
        .transpose()
}

pub fn attach(blob: &[u8], archive: &[u8], page_size: usize) -> Result<Vec<u8>, &'static str> {
    Archive::decode(archive)?;
    if blob.get(8..12) == Some(&super::NATIVE_FORMAT_VERSION.to_le_bytes()) {
        let container = super::NativeContainer::parse(blob)?;
        let mut sections = container.sections;
        if sections.iter().any(|section| section.kind == SEC_ASSETS) {
            return Err("blob already has assets");
        }
        sections.push(Section {
            kind: SEC_ASSETS,
            flags: 0,
            data: archive,
        });
        super::encode_native_aligned(
            container.language,
            container.native_fp,
            container.lumen_version,
            &sections,
            page_size,
        )
    } else {
        let container = super::Container::parse(blob)?;
        let mut sections = container.sections;
        if sections.iter().any(|section| section.kind == SEC_ASSETS) {
            return Err("blob already has assets");
        }
        sections.push(Section {
            kind: SEC_ASSETS,
            flags: 0,
            data: archive,
        });
        super::encode(container.header, &sections)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sorted_bounded_archive() {
        let bytes = encode(&[("b", b"two"), ("a", b"one")]).unwrap();
        assert_eq!(bytes, encode(&[("a", b"one"), ("b", b"two")]).unwrap());
        assert_eq!(
            Archive::decode(&bytes).unwrap().get("a"),
            Some(b"one".as_slice())
        );
        assert!(Archive::decode(&bytes[..bytes.len() - 1]).is_err());
        assert!(encode(&[("../secret", b"")]).is_err());
        assert!(encode(&[("a", b""), ("a", b"")]).is_err());
    }
}
