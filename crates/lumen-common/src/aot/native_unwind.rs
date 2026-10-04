//! Portable unwind recipes. Addresses are supplied only after code placement.
use super::native_data::FunctionEntry;
use crate::target::Arch;

#[derive(Clone, Debug)]
pub struct Record {
    pub function: u32,
    pub cfi: Vec<u8>,
    pub windows: Vec<u8>,
}

pub fn encode(records: &[Record]) -> Result<Vec<u8>, &'static str> {
    let mut out = Vec::new();
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(
        &u32::try_from(records.len())
            .map_err(|_| "too many unwind records")?
            .to_le_bytes(),
    );
    for record in records {
        for n in [
            record.function,
            u32::try_from(record.cfi.len()).map_err(|_| "unwind recipe too large")?,
            u32::try_from(record.windows.len()).map_err(|_| "unwind recipe too large")?,
        ] {
            out.extend_from_slice(&n.to_le_bytes());
        }
        out.extend_from_slice(&record.cfi);
        out.extend_from_slice(&record.windows);
    }
    Ok(out)
}

fn read_u32(bytes: &[u8], at: &mut usize) -> Result<u32, &'static str> {
    let data = bytes
        .get(*at..at.checked_add(4).ok_or("unwind overflow")?)
        .ok_or("truncated unwind data")?;
    *at += 4;
    Ok(u32::from_le_bytes(data.try_into().unwrap()))
}

pub fn decode(bytes: &[u8], functions: &[FunctionEntry]) -> Result<Vec<Record>, &'static str> {
    let mut at = 0;
    if read_u32(bytes, &mut at)? != 1 {
        return Err("unsupported unwind version");
    }
    let count = read_u32(bytes, &mut at)? as usize;
    if count != functions.len() {
        return Err("unwind function count mismatch");
    }
    let mut records = Vec::with_capacity(count.min(bytes.len() / 12));
    for index in 0..count {
        let function = read_u32(bytes, &mut at)?;
        if function as usize != index {
            return Err("unordered unwind functions");
        }
        let cfi_len = read_u32(bytes, &mut at)? as usize;
        let windows_len = read_u32(bytes, &mut at)? as usize;
        let end = at.checked_add(cfi_len).ok_or("unwind overflow")?;
        let cfi = bytes
            .get(at..end)
            .ok_or("truncated unwind recipe")?
            .to_vec();
        at = end;
        let end = at.checked_add(windows_len).ok_or("unwind overflow")?;
        let windows = bytes
            .get(at..end)
            .ok_or("truncated Windows unwind recipe")?
            .to_vec();
        at = end;
        validate_cfi(&cfi, functions[index].len)?;
        if !windows.is_empty()
            && validate_windows(Arch::X86_64, &windows, functions[index].len).is_err()
            && validate_windows(Arch::Aarch64, &windows, functions[index].len).is_err()
        {
            return Err("invalid Windows unwind recipe");
        }
        records.push(Record {
            function,
            cfi,
            windows,
        });
    }
    if at != bytes.len() {
        return Err("trailing unwind data");
    }
    Ok(records)
}

pub fn validate_windows(arch: Arch, bytes: &[u8], function_len: u32) -> Result<(), &'static str> {
    if bytes.len() < 4 || bytes.len() % 4 != 0 {
        return Err("invalid Windows unwind recipe");
    }
    match arch {
        Arch::X86_64 => {
            if bytes[0] != 1
                || 4 + 2 * bytes[2] as usize > bytes.len()
                || bytes[1] as u32 > function_len
            {
                return Err("invalid x64 unwind header");
            }
            validate_x64_windows(bytes)
        }
        Arch::Aarch64 => validate_arm_windows(bytes, function_len),
        _ => Err("unsupported Windows unwind architecture"),
    }
}

fn validate_arm_windows(bytes: &[u8], function_len: u32) -> Result<(), &'static str> {
    let mut at = 0;
    let header = read_u32(bytes, &mut at)?;
    if header & 0x3c0000 != 0 || (header & 0x3ffff) * 4 != function_len {
        return Err("invalid ARM64 unwind header");
    }
    let mut scopes = ((header >> 22) & 31) as usize;
    let mut words = (header >> 27) as usize;
    if scopes == 0 && words == 0 {
        let extended = read_u32(bytes, &mut at)?;
        if extended >> 24 != 0 {
            return Err("invalid extended ARM64 unwind header");
        }
        scopes = (extended & 0xffff) as usize;
        words = ((extended >> 16) & 0xff) as usize;
    }
    let codes_at = at
        .checked_add(scopes.checked_mul(4).ok_or("ARM64 unwind overflow")?)
        .ok_or("ARM64 unwind overflow")?;
    let end = codes_at
        .checked_add(words * 4)
        .ok_or("ARM64 unwind overflow")?;
    if end != bytes.len() || words == 0 {
        return Err("invalid ARM64 unwind size");
    }
    let codes = &bytes[codes_at..end];
    let mut boundaries = Vec::new();
    let mut code_at = 0;
    while code_at < codes.len() {
        boundaries.push(code_at);
        let op = codes[code_at];
        let len = match op {
            0x00..=0x1f | 0x81 | 0xe1 | 0xe3 | 0xe4 => 1,
            0xc0..=0xc7 | 0xd0..=0xd3 | 0xdc..=0xdd => 2,
            0xe0 => 4,
            _ => return Err("unsupported ARM64 unwind instruction"),
        };
        code_at += len;
        if code_at > codes.len() {
            return Err("truncated ARM64 unwind instruction");
        }
        if (0xd0..=0xd3).contains(&op) {
            let register = ((op as u16 & 3) << 2) | (codes[code_at - 1] as u16 >> 6);
            if register > 9 {
                return Err("invalid ARM64 unwind register");
            }
        }
    }
    let instructions = |index: usize| -> Result<u32, &'static str> {
        let start = boundaries
            .binary_search(&index)
            .map_err(|_| "ARM64 unwind index is not an instruction")?;
        for (count, &offset) in boundaries[start..].iter().enumerate() {
            if codes[offset] == 0xe4 {
                return Ok(count as u32 + 1);
            }
        }
        Err("unterminated ARM64 unwind sequence")
    };
    if instructions(0)?.saturating_sub(1) * 4 > function_len {
        return Err("ARM64 prologue outside function");
    }
    let mut last_pc = None;
    for _ in 0..scopes {
        let scope = read_u32(bytes, &mut at)?;
        if scope & 0x3c0000 != 0 {
            return Err("reserved ARM64 epilogue flags");
        }
        let pc = (scope & 0x3ffff) * 4;
        if last_pc.is_some_and(|previous| previous >= pc)
            || pc
                .checked_add(instructions((scope >> 22) as usize)? * 4)
                .is_none_or(|end| end > function_len)
        {
            return Err("ARM64 epilogue outside function");
        }
        last_pc = Some(pc);
    }
    Ok(())
}

fn validate_x64_windows(bytes: &[u8]) -> Result<(), &'static str> {
    let end = 4 + 2 * bytes[2] as usize;
    if bytes[3] != 0 || bytes.len() != (end + 3) & !3 || bytes[end..].iter().any(|byte| *byte != 0)
    {
        return Err("invalid Windows unwind layout");
    }
    let mut at = 4;
    let mut last = bytes[1];
    while at < end {
        let pc = bytes[at];
        let op = bytes[at + 1] & 15;
        let info = bytes[at + 1] >> 4;
        if pc > last {
            return Err("unordered Windows unwind operations");
        }
        last = pc;
        let slots = match (op, info) {
            (0, 3 | 5 | 6 | 7 | 12 | 13 | 14 | 15) => 1,
            (1, 1) => 3,
            (9, 6..=15) => 3,
            _ => return Err("unsupported Windows unwind operation"),
        };
        at += slots * 2;
        if at > end {
            return Err("truncated Windows unwind operation");
        }
    }
    Ok(())
}

fn validate_cfi(bytes: &[u8], len: u32) -> Result<(), &'static str> {
    let mut at = 0;
    let mut pc = 0u32;
    let mut remembered = false;
    while at < bytes.len() {
        let op = bytes[at];
        at += 1;
        let operands = match op {
            4 => {
                pc = pc
                    .checked_add(read_u32(bytes, &mut at)?)
                    .ok_or("unwind PC overflow")?;
                if pc > len {
                    return Err("unwind PC outside function");
                }
                0
            }
            5 | 12 => 2,
            6 => 1,
            10 if !remembered => {
                remembered = true;
                0
            }
            11 if remembered => {
                remembered = false;
                0
            }
            _ => return Err("invalid unwind instruction"),
        };
        for operand in 0..operands {
            let mut value = 0u64;
            let mut shift = 0;
            loop {
                let byte = *bytes.get(at).ok_or("truncated unwind operand")?;
                at += 1;
                if shift >= 63 {
                    return Err("unwind operand overflow");
                }
                value |= ((byte & 127) as u64) << shift;
                if byte & 128 == 0 {
                    break;
                }
                shift += 7;
            }
            if operand == 0 && value > 95 {
                return Err("invalid unwind register");
            }
        }
    }
    if remembered {
        return Err("unbalanced unwind states");
    }
    Ok(())
}

/// `.eh_frame` bytes and `(address field offset, function index)` relocations.
/// Each address field is an absolute 64-bit pointer, also usable by object writers.
pub fn eh_frame(
    arch: Arch,
    functions: &[FunctionEntry],
    records: &[Record],
) -> Result<(Vec<u8>, Vec<(usize, u32)>), &'static str> {
    eh_frame_encoded(arch, functions, records, 0)
}

/// Linker-owned `.eh_frame` using PC-relative signed 64-bit initial locations.
/// Relocations compute `function address - address of the relocated field`.
/// The returned function range fields retain the encoding's eight-byte width.
pub fn eh_frame_pic(
    arch: Arch,
    functions: &[FunctionEntry],
    records: &[Record],
) -> Result<(Vec<u8>, Vec<(usize, u32)>), &'static str> {
    eh_frame_encoded(arch, functions, records, 0x1c)
}

fn eh_frame_encoded(
    arch: Arch,
    functions: &[FunctionEntry],
    records: &[Record],
    encoding: u8,
) -> Result<(Vec<u8>, Vec<(usize, u32)>), &'static str> {
    if records.len() != functions.len()
        || records
            .iter()
            .enumerate()
            .any(|(index, record)| record.function as usize != index)
    {
        return Err("unordered unwind functions");
    }
    let (sp, ret, initial) = match arch {
        Arch::X86_64 => (7, 16, vec![12, 7, 8, 0x90, 1]),
        Arch::Aarch64 => (31, 30, vec![12, 31, 0]),
        _ => return Err("unwind architecture unsupported"),
    };
    let _ = sp;
    let mut out = vec![0; 4];
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&[1, b'z', b'R', 0, 1, 0x78, ret, 1, encoding]);
    out.extend_from_slice(&initial);
    while out.len() % 8 != 0 {
        out.push(0);
    }
    let cie_len = out.len() as u32 - 4;
    out[..4].copy_from_slice(&cie_len.to_le_bytes());
    let mut relocations = Vec::new();
    for record in records {
        let function = functions
            .get(record.function as usize)
            .ok_or("unwind function out of range")?;
        // Folded functions must produce only one overlapping FDE.
        if records[..record.function as usize]
            .iter()
            .any(|r| functions[r.function as usize].offset == function.offset)
        {
            continue;
        }
        let start = out.len();
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&((start + 4) as u32).to_le_bytes());
        relocations.push((out.len(), record.function));
        out.extend_from_slice(&0u64.to_le_bytes());
        out.extend_from_slice(&(function.len as u64).to_le_bytes());
        out.push(0); // Empty FDE augmentation.
        out.extend_from_slice(&record.cfi);
        while out.len() % 8 != 0 {
            out.push(0);
        }
        let length = (out.len() - start - 4) as u32;
        out[start..start + 4].copy_from_slice(&length.to_le_bytes());
    }
    out.extend_from_slice(&0u32.to_le_bytes());
    Ok((out, relocations))
}
