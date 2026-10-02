//! Optional on-device source locations for a native image.
//! Stripped images omit this section and keep a hash-bound host sidecar instead.

use super::native_data::FunctionEntry;

pub const MAGIC: &[u8; 8] = b"LUMLIN01";
pub const VERSION: u32 = 1;
const HEADER_LEN: usize = 20;
const LOCATION_LEN: usize = 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Location {
    pub function: u32,
    pub code_offset: u32,
    pub file: u32,
    pub line: u32,
    pub column: u32,
}

pub struct NativeLines<'a> {
    pub files: Vec<&'a str>,
    pub locations: Vec<Location>,
}

impl NativeLines<'_> {
    pub fn lookup(&self, function: u32, code_offset: u32) -> Option<(&str, u32, u32)> {
        let at = self
            .locations
            .partition_point(|loc| (loc.function, loc.code_offset) <= (function, code_offset));
        let location = self.locations.get(at.checked_sub(1)?)?;
        (location.function == function)
            .then_some(location)
            .and_then(|location| {
                self.files
                    .get(location.file as usize)
                    .map(|file| (*file, location.line, location.column))
            })
    }
}

pub fn encode(
    files: &[&str],
    locations: &[Location],
    functions: &[FunctionEntry],
) -> Result<Vec<u8>, &'static str> {
    validate(files, locations, functions)?;
    let file_count = u32::try_from(files.len()).map_err(|_| "too many native source files")?;
    let location_count =
        u32::try_from(locations.len()).map_err(|_| "too many native source locations")?;
    let capacity = files
        .iter()
        .try_fold(HEADER_LEN, |size, name| {
            size.checked_add(4)?.checked_add(name.len())
        })
        .and_then(|size| size.checked_add(locations.len().checked_mul(LOCATION_LEN)?))
        .ok_or("native line table too large")?;
    let mut out = Vec::with_capacity(capacity);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&file_count.to_le_bytes());
    out.extend_from_slice(&location_count.to_le_bytes());
    for file in files {
        let len = u32::try_from(file.len()).map_err(|_| "native source filename too long")?;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(file.as_bytes());
    }
    for location in locations {
        for word in [
            location.function,
            location.code_offset,
            location.file,
            location.line,
            location.column,
        ] {
            out.extend_from_slice(&word.to_le_bytes());
        }
    }
    Ok(out)
}

pub fn decode<'a>(
    bytes: &'a [u8],
    functions: &[FunctionEntry],
) -> Result<NativeLines<'a>, &'static str> {
    if bytes.len() < HEADER_LEN || &bytes[..8] != MAGIC {
        return Err("invalid native line table header");
    }
    let u32_at = |at| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
    if u32_at(8) != VERSION {
        return Err("native line table version mismatch");
    }
    let file_count = u32_at(12) as usize;
    let location_count = u32_at(16) as usize;
    let location_bytes = location_count
        .checked_mul(LOCATION_LEN)
        .ok_or("native line table too large")?;
    let location_start = bytes
        .len()
        .checked_sub(location_bytes)
        .filter(|&at| at >= HEADER_LEN)
        .ok_or("truncated native line table")?;
    if file_count > (location_start - HEADER_LEN) / 4 {
        return Err("truncated native source names");
    }
    let mut files = Vec::with_capacity(file_count);
    let mut at = HEADER_LEN;
    for _ in 0..file_count {
        let len_end = at
            .checked_add(4)
            .filter(|&end| end <= location_start)
            .ok_or("truncated native source name")?;
        let len = u32::from_le_bytes(bytes[at..len_end].try_into().unwrap()) as usize;
        at = len_end;
        let end = at
            .checked_add(len)
            .filter(|&end| end <= location_start)
            .ok_or("truncated native source name")?;
        let name =
            std::str::from_utf8(&bytes[at..end]).map_err(|_| "invalid native source name")?;
        files.push(name);
        at = end;
    }
    if at != location_start {
        return Err("native line table has trailing source data");
    }
    let mut locations = Vec::with_capacity(location_count);
    for record in bytes[location_start..].chunks_exact(LOCATION_LEN) {
        let word = |at| u32::from_le_bytes(record[at..at + 4].try_into().unwrap());
        locations.push(Location {
            function: word(0),
            code_offset: word(4),
            file: word(8),
            line: word(12),
            column: word(16),
        });
    }
    validate(&files, &locations, functions)?;
    Ok(NativeLines { files, locations })
}

fn validate(
    files: &[&str],
    locations: &[Location],
    functions: &[FunctionEntry],
) -> Result<(), &'static str> {
    if files
        .iter()
        .any(|name| !super::normalized_source_path(name))
    {
        return Err("native source names must be relative, normalized slash paths");
    }
    if locations.iter().any(|location| {
        location.file as usize >= files.len()
            || location.line == 0
            || functions
                .get(location.function as usize)
                .is_none_or(|function| location.code_offset >= function.len)
    }) || locations.windows(2).any(|pair| {
        (pair[0].function, pair[0].code_offset) >= (pair[1].function, pair[1].code_offset)
    }) {
        return Err("invalid native source location");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_locations_outside_exact_function_range() {
        let functions = [FunctionEntry { offset: 0, len: 5 }];
        let location = Location {
            function: 0,
            code_offset: 4,
            file: 0,
            line: 12,
            column: 3,
        };
        let encoded = encode(&["src/app.js"], &[location], &functions).unwrap();
        let parsed = decode(&encoded, &functions).unwrap();
        assert_eq!(parsed.lookup(0, 4), Some(("src/app.js", 12, 3)));
        assert!(encode(
            &["src/app.js"],
            &[Location {
                code_offset: 5,
                ..location
            }],
            &functions
        )
        .is_err());
        let mut invalid = encoded;
        let at = invalid.len() - LOCATION_LEN + 4;
        invalid[at..at + 4].copy_from_slice(&5u32.to_le_bytes());
        assert!(decode(&invalid, &functions).is_err());
        assert!(encode(&["../app.js"], &[location], &functions).is_err());
    }
}
