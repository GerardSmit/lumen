//! Deterministic standalone stub payloads. No OS calls or language-specific data.

use std::ops::Range;

const ELF_NOTE: u32 = 0x4c55_4d01;

#[cfg(test)]
mod tests {
    use super::*;

    fn elf_stub() -> Vec<u8> {
        let mut bytes = vec![0; 208];
        bytes[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
        bytes[16..18].copy_from_slice(&2u16.to_le_bytes());
        bytes[18..20].copy_from_slice(&62u16.to_le_bytes());
        bytes[24..32].copy_from_slice(&4096u64.to_le_bytes());
        bytes[40..48].copy_from_slice(&80u64.to_le_bytes());
        for (offset, value) in [(52, 64u16), (58, 64), (60, 2), (62, 1)] {
            bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
        }
        bytes[64..75].copy_from_slice(b"\0.shstrtab\0");
        bytes[144..148].copy_from_slice(&1u32.to_le_bytes());
        bytes[148..152].copy_from_slice(&3u32.to_le_bytes());
        bytes[168..176].copy_from_slice(&64u64.to_le_bytes());
        bytes[176..184].copy_from_slice(&11u64.to_le_bytes());
        bytes
    }

    #[test]
    fn elf_embedding_is_deterministic_and_checked() {
        let stub = elf_stub();
        assert_eq!(locate_elf_blob(&stub).unwrap(), None);
        assert_eq!(architecture(&stub).unwrap(), crate::target::Arch::X86_64);
        let packed = embed_elf(&stub, b"app payload").unwrap();
        assert_eq!(packed, embed_elf(&stub, b"app payload").unwrap());
        let range = locate_elf_blob(&packed).unwrap().unwrap();
        assert_eq!(&packed[range], b"app payload");
        assert!(embed_elf(&packed, b"second app").is_err());
        assert!(locate_elf_blob(&packed[..packed.len() - 1]).is_err());
        assert!(embed_elf(&stub, b"").is_err());
    }

    #[test]
    fn macho_embedding_preserves_linkedit_and_rejects_truncation() {
        let mut stub = vec![0; 2048];
        for (offset, value) in [
            (0, 0xfeed_facfu32),
            (4, 0x0100_0007),
            (12, 2),
            (16, 2),
            (20, 144),
        ] {
            stub[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        for (offset, name, vm, file) in [
            (32, b"__TEXT".as_slice(), 0u64, 0u64),
            (104, b"__LINKEDIT".as_slice(), 4096, 1024),
        ] {
            stub[offset..offset + 4].copy_from_slice(&0x19u32.to_le_bytes());
            stub[offset + 4..offset + 8].copy_from_slice(&72u32.to_le_bytes());
            stub[offset + 8..offset + 8 + name.len()].copy_from_slice(name);
            stub[offset + 24..offset + 32].copy_from_slice(&vm.to_le_bytes());
            stub[offset + 32..offset + 40].copy_from_slice(&4096u64.to_le_bytes());
            stub[offset + 40..offset + 48].copy_from_slice(&file.to_le_bytes());
            stub[offset + 48..offset + 56].copy_from_slice(&1024u64.to_le_bytes());
        }
        stub[1024..].fill(0x5a);
        let packed = embed_macho(&stub, b"native payload").unwrap();
        assert_eq!(packed, embed_macho(&stub, b"native payload").unwrap());
        assert_eq!(
            &packed[locate_macho_blob(&packed).unwrap().unwrap()],
            b"native payload"
        );
        let command = macho_commands(&packed)
            .unwrap()
            .into_iter()
            .find(|&(at, _, _)| fixed_name(&packed[at + 8..at + 24], b"__LINKEDIT"))
            .unwrap()
            .0;
        let offset = wide(&packed, command + 40).unwrap();
        assert_eq!(&packed[offset..offset + 1024], &stub[1024..]);
        assert!(embed_macho(&packed, b"second").is_err());
        assert!(embed_macho(&stub[..1200], b"app").is_err());
    }
}

pub fn pe_header(bytes: &[u8]) -> Result<usize, String> {
    if take(bytes, 0, 2)? != b"MZ" {
        return Err("stub is not a PE executable".into());
    }
    let header = word(bytes, 60)? as usize;
    if take(bytes, header, 4)? != b"PE\0\0"
        || short(bytes, header + 24)? != 0x20b
        || short(bytes, header + 22)? & 0x2002 != 2
    {
        return Err("stub requires a PE32+ executable".into());
    }
    let optional_len = short(bytes, header + 20)?;
    if optional_len < 152 {
        return Err("truncated PE optional header".into());
    }
    take(bytes, header + 24, optional_len)?;
    Ok(header)
}

/// Convert an ICO directory into a Windows group resource and its image resources.
pub fn icon_resources(bytes: &[u8]) -> Result<(Vec<u8>, Vec<&[u8]>), String> {
    if short(bytes, 0)? != 0 || short(bytes, 2)? != 1 {
        return Err("icon requires an ICO file".into());
    }
    let count = short(bytes, 4)?;
    if count == 0 {
        return Err("ICO file has no images".into());
    }
    take(
        bytes,
        6,
        count.checked_mul(16).ok_or("ICO directory overflow")?,
    )?;
    let mut group = bytes[..6].to_vec();
    let mut images = Vec::with_capacity(count);
    for index in 0..count {
        let entry = 6 + index * 16;
        if bytes[entry + 3] != 0 {
            return Err("invalid ICO directory entry".into());
        }
        let offset = word(bytes, entry + 12)? as usize;
        let length = word(bytes, entry + 8)? as usize;
        if length == 0 || offset < 6 + count * 16 {
            return Err("invalid ICO image range".into());
        }
        images.push(take(bytes, offset, length)?);
        group.extend_from_slice(&bytes[entry..entry + 12]);
        group.extend_from_slice(
            &u16::try_from(index + 1)
                .map_err(|_| "too many ICO images")?
                .to_le_bytes(),
        );
    }
    Ok((group, images))
}

pub fn architecture(bytes: &[u8]) -> Result<crate::target::Arch, String> {
    use crate::target::Arch;
    let machine = if bytes.starts_with(b"MZ") {
        match short(bytes, pe_header(bytes)? + 4)? {
            0x8664 => 0,
            0xaa64 => 1,
            _ => 2,
        }
    } else if bytes.starts_with(b"\x7fELF") {
        elf(bytes)?;
        if !matches!(short(bytes, 16)?, 2 | 3) || wide(bytes, 24)? == 0 {
            return Err("ELF stub is not an executable".into());
        }
        match short(bytes, 18)? {
            62 => 0,
            183 => 1,
            _ => 2,
        }
    } else if bytes.starts_with(&0xfeed_facfu32.to_le_bytes()) {
        macho_commands(bytes)?;
        match word(bytes, 4)? {
            0x0100_0007 => 0,
            0x0100_000c => 1,
            _ => 2,
        }
    } else {
        return Err(
            "unsupported standalone stub format; expected PE32+, ELF64 or thin Mach-O64".into(),
        );
    };
    match machine {
        0 => Ok(Arch::X86_64),
        1 => Ok(Arch::Aarch64),
        _ => Err("unsupported standalone stub architecture".into()),
    }
}

fn take(bytes: &[u8], at: usize, len: usize) -> Result<&[u8], String> {
    bytes
        .get(at..at.checked_add(len).ok_or("executable offset overflow")?)
        .ok_or_else(|| "truncated executable".into())
}
fn word(bytes: &[u8], at: usize) -> Result<u32, String> {
    Ok(u32::from_le_bytes(take(bytes, at, 4)?.try_into().unwrap()))
}
fn wide(bytes: &[u8], at: usize) -> Result<usize, String> {
    usize::try_from(u64::from_le_bytes(take(bytes, at, 8)?.try_into().unwrap()))
        .map_err(|_| "executable offset too large".into())
}
fn short(bytes: &[u8], at: usize) -> Result<usize, String> {
    Ok(u16::from_le_bytes(take(bytes, at, 2)?.try_into().unwrap()) as usize)
}
fn align(bytes: &mut Vec<u8>, alignment: usize) -> Result<(), String> {
    let end = bytes
        .len()
        .checked_add(alignment - 1)
        .ok_or("executable size overflow")?
        & !(alignment - 1);
    bytes.resize(end, 0);
    Ok(())
}

fn elf(bytes: &[u8]) -> Result<(usize, usize, usize), String> {
    if take(bytes, 0, 7)? != b"\x7fELF\x02\x01\x01" {
        return Err("stub requires little-endian ELF64".into());
    }
    if short(bytes, 52)? != 64 || short(bytes, 58)? != 64 {
        return Err("unsupported ELF header size".into());
    }
    let (offset, count, names) = (wide(bytes, 40)?, short(bytes, 60)?, short(bytes, 62)?);
    if count == 0 || names >= count {
        return Err("ELF stub needs a nonextended section table".into());
    }
    take(
        bytes,
        offset,
        count.checked_mul(64).ok_or("ELF section count overflow")?,
    )?;
    Ok((offset, count, names))
}

fn elf_name<'a>(bytes: &[u8], table: &'a [u8], header: usize) -> Result<&'a str, String> {
    let at = word(bytes, header)? as usize;
    let name = table.get(at..).ok_or("invalid ELF section name")?;
    let end = name
        .iter()
        .position(|byte| *byte == 0)
        .ok_or("unterminated ELF section name")?;
    std::str::from_utf8(&name[..end]).map_err(|_| "invalid ELF section name".into())
}

pub fn locate_elf_blob(bytes: &[u8]) -> Result<Option<Range<usize>>, String> {
    let (headers, count, names) = elf(bytes)?;
    let names = headers + names * 64;
    let table = take(bytes, wide(bytes, names + 24)?, wide(bytes, names + 32)?)?;
    for index in 0..count {
        let header = headers + index * 64;
        if elf_name(bytes, table, header)? != ".note.lumen" {
            continue;
        }
        if word(bytes, header + 4)? != 7 {
            return Err("invalid Lumen ELF note section".into());
        }
        let note = take(bytes, wide(bytes, header + 24)?, wide(bytes, header + 32)?)?;
        if word(note, 0)? != 6
            || word(note, 4)? != 16
            || word(note, 8)? != ELF_NOTE
            || take(note, 12, 8)? != b"LUMEN\0\0\0"
        {
            return Err("invalid Lumen ELF note".into());
        }
        let offset = wide(note, 20)?;
        let length = wide(note, 28)?;
        take(bytes, offset, length)?;
        return Ok(Some(offset..offset + length));
    }
    Ok(None)
}

pub fn embed_elf(stub: &[u8], blob: &[u8]) -> Result<Vec<u8>, String> {
    if blob.is_empty() {
        return Err("empty standalone blob".into());
    }
    if locate_elf_blob(stub)?.is_some() {
        return Err("ELF stub already contains an app".into());
    }
    let (headers, count, names) = elf(stub)?;
    if count > u16::MAX as usize - 2 {
        return Err("too many ELF sections".into());
    }
    let names_header = headers + names * 64;
    let mut strings = take(
        stub,
        wide(stub, names_header + 24)?,
        wide(stub, names_header + 32)?,
    )?
    .to_vec();
    let blob_name = u32::try_from(strings.len()).map_err(|_| "ELF string table too large")?;
    strings.extend_from_slice(b".lumen_blob\0");
    let note_name = u32::try_from(strings.len()).map_err(|_| "ELF string table too large")?;
    strings.extend_from_slice(b".note.lumen\0");
    let mut output = stub.to_vec();
    align(&mut output, 4096)?;
    let blob_offset = output.len();
    output.extend_from_slice(blob);
    align(&mut output, 4)?;
    let note_offset = output.len();
    for word in [6u32, 16, ELF_NOTE] {
        output.extend_from_slice(&word.to_le_bytes());
    }
    output.extend_from_slice(b"LUMEN\0\0\0");
    output.extend_from_slice(&(blob_offset as u64).to_le_bytes());
    output.extend_from_slice(&(blob.len() as u64).to_le_bytes());
    let strings_offset = output.len();
    output.extend_from_slice(&strings);
    align(&mut output, 8)?;
    let new_headers = output.len();
    output.extend_from_slice(take(stub, headers, count * 64)?);
    let update = new_headers + names * 64;
    output[update + 24..update + 32].copy_from_slice(&(strings_offset as u64).to_le_bytes());
    output[update + 32..update + 40].copy_from_slice(&(strings.len() as u64).to_le_bytes());
    for (name, kind, offset, len, alignment) in [
        (blob_name, 1u32, blob_offset, blob.len(), 4096u64),
        (note_name, 7, note_offset, 36, 4),
    ] {
        let mut header = [0u8; 64];
        header[..4].copy_from_slice(&name.to_le_bytes());
        header[4..8].copy_from_slice(&kind.to_le_bytes());
        header[24..32].copy_from_slice(&(offset as u64).to_le_bytes());
        header[32..40].copy_from_slice(&(len as u64).to_le_bytes());
        header[48..56].copy_from_slice(&alignment.to_le_bytes());
        output.extend_from_slice(&header);
    }
    output[40..48].copy_from_slice(&(new_headers as u64).to_le_bytes());
    output[60..62].copy_from_slice(&((count + 2) as u16).to_le_bytes());
    Ok(output)
}

fn macho_commands(bytes: &[u8]) -> Result<Vec<(usize, usize, u32)>, String> {
    if word(bytes, 0)? != 0xfeed_facf || word(bytes, 12)? != 2 {
        return Err("stub requires a thin little-endian Mach-O64 executable".into());
    }
    let count = word(bytes, 16)? as usize;
    let end = 32usize
        .checked_add(word(bytes, 20)? as usize)
        .ok_or("Mach-O command overflow")?;
    take(bytes, 32, end - 32)?;
    if count > (end - 32) / 8 {
        return Err("invalid Mach-O command count".into());
    }
    let mut commands = Vec::with_capacity(count);
    let mut at = 32;
    for _ in 0..count {
        let len = word(bytes, at + 4)? as usize;
        if len < 8 || len % 8 != 0 || at.checked_add(len).is_none_or(|n| n > end) {
            return Err("invalid Mach-O load command".into());
        }
        commands.push((at, len, word(bytes, at)?));
        at += len;
    }
    if at != end {
        return Err("Mach-O command lengths disagree".into());
    }
    Ok(commands)
}

fn fixed_name(bytes: &[u8], name: &[u8]) -> bool {
    bytes.starts_with(name) && bytes.get(name.len()).is_some_and(|b| *b == 0)
}

pub fn locate_macho_blob(bytes: &[u8]) -> Result<Option<Range<usize>>, String> {
    for (at, len, kind) in macho_commands(bytes)? {
        if kind != 0x19 {
            continue;
        }
        if len < 72 {
            return Err("truncated Mach-O segment".into());
        }
        let count = word(bytes, at + 64)? as usize;
        if count > (len - 72) / 80 {
            return Err("invalid Mach-O section count".into());
        }
        for index in 0..count {
            let section = at + 72 + index * 80;
            if fixed_name(take(bytes, section, 16)?, b"__blob")
                && fixed_name(take(bytes, section + 16, 16)?, b"__LUMEN")
            {
                let offset = word(bytes, section + 48)? as usize;
                let size = wide(bytes, section + 40)?;
                take(bytes, offset, size)?;
                return Ok(Some(offset..offset + size));
            }
        }
    }
    Ok(None)
}

/// Move LINKEDIT behind a new read-only __LUMEN,__blob segment. The OS wrapper
/// must ad-hoc sign the result on macOS before publishing it.
pub fn embed_macho(stub: &[u8], blob: &[u8]) -> Result<Vec<u8>, String> {
    if blob.is_empty() {
        return Err("empty standalone blob".into());
    }
    if locate_macho_blob(stub)?.is_some() {
        return Err("Mach-O stub already contains an app".into());
    }
    let commands = macho_commands(stub)?;
    if commands.iter().any(|&(_, _, kind)| kind == 0x8000_0034) {
        return Err("Mach-O chained-fixup stubs require relinking with classic dyld fixups before embedding".into());
    }
    let linkedit = commands
        .iter()
        .find(|&&(at, len, kind)| {
            kind == 0x19 && len >= 72 && fixed_name(&stub[at + 8..at + 24], b"__LINKEDIT")
        })
        .ok_or("Mach-O stub needs a LINKEDIT segment")?
        .0;
    let old_offset = wide(stub, linkedit + 40)?;
    let old_len = wide(stub, linkedit + 48)?;
    let old_vm = wide(stub, linkedit + 24)?;
    let linkedit_bytes = take(stub, old_offset, old_len)?;
    let page = if word(stub, 4)? == 0x0100_000c {
        16384
    } else {
        4096
    };
    let mut first_data = old_offset;
    for &(at, len, kind) in &commands {
        if kind == 0x19 {
            if len < 72 {
                return Err("truncated Mach-O segment".into());
            }
            let sections = word(stub, at + 64)? as usize;
            if sections > (len - 72) / 80 {
                return Err("invalid Mach-O section count".into());
            }
            for index in 0..sections {
                let offset = word(stub, at + 72 + index * 80 + 48)? as usize;
                if offset != 0 {
                    first_data = first_data.min(offset);
                }
            }
        }
    }
    let mut output = stub.to_vec();
    align(&mut output, page)?;
    let blob_offset = output.len();
    output.extend_from_slice(blob);
    align(&mut output, page)?;
    let new_offset = output.len();
    output.extend_from_slice(linkedit_bytes);
    let vm_len = new_offset - blob_offset;
    let new_vm = old_vm
        .checked_add(vm_len)
        .ok_or("Mach-O VM range overflow")?;
    let delta = new_offset
        .checked_sub(old_offset)
        .ok_or("Mach-O LINKEDIT range overflow")?;
    let mut load_commands = Vec::new();
    let mut command_count = 0u32;
    for &(at, len, kind) in &commands {
        if kind == 0x1d {
            continue;
        } // stale code signature; wrapper re-signs
        if at == linkedit {
            let mut segment = [0u8; 152];
            segment[..4].copy_from_slice(&0x19u32.to_le_bytes());
            segment[4..8].copy_from_slice(&152u32.to_le_bytes());
            segment[8..15].copy_from_slice(b"__LUMEN");
            segment[24..32].copy_from_slice(&(old_vm as u64).to_le_bytes());
            segment[32..40].copy_from_slice(&(vm_len as u64).to_le_bytes());
            segment[40..48].copy_from_slice(&(blob_offset as u64).to_le_bytes());
            segment[48..56].copy_from_slice(&(blob.len() as u64).to_le_bytes());
            segment[56..60].copy_from_slice(&1u32.to_le_bytes());
            segment[60..64].copy_from_slice(&1u32.to_le_bytes());
            segment[64..68].copy_from_slice(&1u32.to_le_bytes());
            segment[72..78].copy_from_slice(b"__blob");
            segment[88..95].copy_from_slice(b"__LUMEN");
            segment[104..112].copy_from_slice(&(old_vm as u64).to_le_bytes());
            segment[112..120].copy_from_slice(&(blob.len() as u64).to_le_bytes());
            segment[120..124].copy_from_slice(
                &u32::try_from(blob_offset)
                    .map_err(|_| "Mach-O exceeds 4 GiB")?
                    .to_le_bytes(),
            );
            segment[124..128]
                .copy_from_slice(&(if page == 16384 { 14u32 } else { 12 }).to_le_bytes());
            load_commands.extend_from_slice(&segment);
            command_count += 1;
        }
        let mut command = take(stub, at, len)?.to_vec();
        let offsets: &[usize] = match kind {
            2 => &[8, 16],
            0xb => &[32, 40, 48, 56, 64, 72],
            0x22 | 0x8000_0022 => &[8, 16, 24, 32, 40],
            0x16 | 0x1e | 0x26 | 0x29 | 0x2b | 0x2e | 0x8000_0033 | 0x8000_0034 => &[8],
            _ => &[],
        };
        for &field in offsets {
            let value = word(&command, field)? as usize;
            if value == 0 {
                continue;
            }
            if value < old_offset
                || value
                    >= old_offset
                        .checked_add(old_len)
                        .ok_or("Mach-O LINKEDIT range overflow")?
            {
                return Err("Mach-O table is outside LINKEDIT".into());
            }
            let updated = u32::try_from(value.checked_add(delta).ok_or("Mach-O table overflow")?)
                .map_err(|_| "Mach-O exceeds 4 GiB")?;
            command[field..field + 4].copy_from_slice(&updated.to_le_bytes());
        }
        if at == linkedit {
            if word(&command, 64)? != 0 {
                return Err("Mach-O LINKEDIT sections are unsupported".into());
            }
            command[24..32].copy_from_slice(&(new_vm as u64).to_le_bytes());
            command[40..48].copy_from_slice(&(new_offset as u64).to_le_bytes());
        }
        load_commands.extend(command);
        command_count += 1;
    }
    let end = 32usize
        .checked_add(load_commands.len())
        .ok_or("Mach-O load command overflow")?;
    if end > first_data {
        return Err("Mach-O stub needs more header padding for __LUMEN".into());
    }
    output[16..20].copy_from_slice(&command_count.to_le_bytes());
    output[20..24].copy_from_slice(
        &u32::try_from(load_commands.len())
            .map_err(|_| "Mach-O command table too large")?
            .to_le_bytes(),
    );
    let old_end = 32 + word(stub, 20)? as usize;
    output[32..end.max(old_end)].fill(0);
    output[32..end].copy_from_slice(&load_commands);
    Ok(output)
}
