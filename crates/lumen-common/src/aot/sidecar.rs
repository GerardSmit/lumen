//! Host-side line maps for stripped native blobs.

use sha2::{Digest, Sha256};

pub const MAGIC: &[u8; 8] = b"LUMMAP01";
pub const VERSION: u32 = 1;
const HEADER_LEN: usize = 52;
const ENTRY_LEN: usize = 20;

pub use super::native_lines::Location;

#[derive(Debug)]
pub struct Sidecar {
    pub blob_hash: [u8; 32],
    pub files: Vec<String>,
    pub locations: Vec<Location>,
}

impl Sidecar {
    pub fn new(
        blob: &[u8],
        files: Vec<String>,
        locations: Vec<Location>,
    ) -> Result<Self, &'static str> {
        let map = Self {
            blob_hash: hash(blob),
            files,
            locations,
        };
        map.validate()?;
        map.validate_for_blob(blob)?;
        Ok(map)
    }

    pub fn matches(&self, blob: &[u8]) -> bool {
        self.blob_hash == hash(blob)
    }

    /// Check the hash and every function/PC against the referenced native blob.
    pub fn validate_for_blob(&self, blob: &[u8]) -> Result<(), &'static str> {
        self.validate()?;
        if !self.matches(blob) {
            return Err("native line map blob hash mismatch");
        }
        let container = super::NativeContainer::parse(blob)?;
        for loc in &self.locations {
            let function = container
                .functions
                .get(loc.function as usize)
                .ok_or("line function out of range")?;
            if loc.code_offset >= function.len {
                return Err("line code offset out of range");
            }
        }
        Ok(())
    }

    pub fn lookup(&self, function: u32, code_offset: u32) -> Option<(&str, u32, u32)> {
        let at = self
            .locations
            .partition_point(|loc| (loc.function, loc.code_offset) <= (function, code_offset));
        let loc = self.locations.get(at.checked_sub(1)?)?;
        (loc.function == function).then_some(loc).and_then(|loc| {
            self.files
                .get(loc.file as usize)
                .map(|file| (file.as_str(), loc.line, loc.column))
        })
    }

    pub fn encode(&self) -> Result<Vec<u8>, &'static str> {
        self.validate()?;
        let files = u32::try_from(self.files.len()).map_err(|_| "too many source files")?;
        let locations = u32::try_from(self.locations.len()).map_err(|_| "too many line entries")?;
        let capacity = self
            .files
            .iter()
            .try_fold(HEADER_LEN, |n, file| {
                n.checked_add(4)?.checked_add(file.len())
            })
            .and_then(|n| n.checked_add(self.locations.len().checked_mul(ENTRY_LEN)?))
            .ok_or("native line map too large")?;
        let mut out = Vec::with_capacity(capacity);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&self.blob_hash);
        out.extend_from_slice(&files.to_le_bytes());
        out.extend_from_slice(&locations.to_le_bytes());
        for file in &self.files {
            let len = u32::try_from(file.len()).map_err(|_| "source filename too long")?;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(file.as_bytes());
        }
        for loc in &self.locations {
            for word in [
                loc.function,
                loc.code_offset,
                loc.file,
                loc.line,
                loc.column,
            ] {
                out.extend_from_slice(&word.to_le_bytes());
            }
        }
        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, &'static str> {
        if bytes.len() < HEADER_LEN || &bytes[..8] != MAGIC {
            return Err("invalid native line map header");
        }
        let u32_at = |at| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
        if u32_at(8) != VERSION {
            return Err("native line map version mismatch");
        }
        let blob_hash = bytes[12..44].try_into().unwrap();
        let files = u32_at(44) as usize;
        let locations = u32_at(48) as usize;
        let entries_len = locations
            .checked_mul(ENTRY_LEN)
            .ok_or("native line map too large")?;
        let file_end = bytes
            .len()
            .checked_sub(entries_len)
            .filter(|&n| n >= HEADER_LEN)
            .ok_or("truncated native line map")?;
        if files > (file_end - HEADER_LEN) / 4 {
            return Err("truncated source names");
        }
        let mut pos = HEADER_LEN;
        let mut names = Vec::with_capacity(files);
        for _ in 0..files {
            let end = pos
                .checked_add(4)
                .filter(|&n| n <= file_end)
                .ok_or("truncated source name")?;
            let len = u32::from_le_bytes(bytes[pos..end].try_into().unwrap()) as usize;
            pos = end;
            let end = pos
                .checked_add(len)
                .filter(|&n| n <= file_end)
                .ok_or("truncated source name")?;
            names.push(
                std::str::from_utf8(&bytes[pos..end])
                    .map_err(|_| "invalid source name")?
                    .to_owned(),
            );
            pos = end;
        }
        if pos != file_end {
            return Err("native line map has trailing file data");
        }
        let mut lines = Vec::with_capacity(locations);
        for record in bytes[file_end..].chunks_exact(ENTRY_LEN) {
            let word = |at| u32::from_le_bytes(record[at..at + 4].try_into().unwrap());
            lines.push(Location {
                function: word(0),
                code_offset: word(4),
                file: word(8),
                line: word(12),
                column: word(16),
            });
        }
        let map = Self {
            blob_hash,
            files: names,
            locations: lines,
        };
        map.validate()?;
        Ok(map)
    }

    fn validate(&self) -> Result<(), &'static str> {
        if self
            .files
            .iter()
            .any(|name| !super::normalized_source_path(name))
        {
            return Err("native line-map paths must be relative, normalized slash paths");
        }
        if self
            .locations
            .iter()
            .any(|loc| loc.file as usize >= self.files.len() || loc.line == 0)
            || self
                .locations
                .windows(2)
                .any(|w| (w[0].function, w[0].code_offset) >= (w[1].function, w[1].code_offset))
        {
            return Err("invalid native line entries");
        }
        Ok(())
    }
}

pub fn hash(blob: &[u8]) -> [u8; 32] {
    let digest = Sha256::digest(blob);
    let mut out = [0; 32];
    out.copy_from_slice(&digest);
    out
}

/// Hash the complete source set used to build an installable app.
/// Names are paths relative to the source root, with `/` separators.
pub fn hash_sources(sources: &[(&str, &[u8])]) -> Result<[u8; 32], &'static str> {
    if sources
        .iter()
        .any(|(name, _)| !super::normalized_source_path(name))
    {
        return Err("source names must be relative, normalized slash paths");
    }
    let mut sorted = sources.to_vec();
    sorted.sort_unstable_by(|a, b| a.0.cmp(b.0));
    if sorted.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        return Err("duplicate source name");
    }
    let mut digest = Sha256::new();
    digest.update(b"LUMEN-AOT-SOURCE-SET-1");
    digest.update((sorted.len() as u64).to_le_bytes());
    for (name, bytes) in sorted {
        digest.update((name.len() as u64).to_le_bytes());
        digest.update(name.as_bytes());
        digest.update((bytes.len() as u64).to_le_bytes());
        digest.update(bytes);
    }
    let mut out = [0; 32];
    out.copy_from_slice(&digest.finalize());
    Ok(out)
}
