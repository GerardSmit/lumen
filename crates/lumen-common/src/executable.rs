//! Deterministic standalone stub payloads. No OS calls or language-specific data.
//!
//! ELF, Mach-O and PE structure comes from the `object` crate; only the byte-exact patching of
//! the stub (appending the blob, a section/segment header and relocating LINKEDIT) is done here.

use object::endian::{LittleEndian as LE, U32, U64};
use object::read::elf::{FileHeader as _, SectionHeader as _};
use object::read::macho::{LoadCommandData, MachHeader as _, Section as _, Segment as _};
use object::read::pe::{DataDirectories, ImageNtHeaders as _};
use object::{elf, macho, pe, pod, Architecture, FileKind, Object};
use std::ops::Range;

const ELF_NOTE: u32 = 0x4c55_4d01;

type ElfHeader = elf::FileHeader64<LE>;
type MachHeader = macho::MachHeader64<LE>;

fn err(error: object::Error) -> String {
    error.to_string()
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

fn pe_nt_headers(
    bytes: &[u8],
) -> Result<(usize, &pe::ImageNtHeaders64, DataDirectories<'_>), String> {
    if FileKind::parse(bytes) != Ok(FileKind::Pe64) {
        return Err("stub requires a PE32+ executable".into());
    }
    let header = pe::ImageDosHeader::parse(bytes)
        .map_err(err)?
        .nt_headers_offset();
    let mut offset = u64::from(header);
    let (nt, directories) = pe::ImageNtHeaders64::parse(bytes, &mut offset).map_err(err)?;
    let characteristics = nt.file_header().characteristics.get(LE);
    if characteristics & (pe::IMAGE_FILE_EXECUTABLE_IMAGE | pe::IMAGE_FILE_DLL)
        != pe::IMAGE_FILE_EXECUTABLE_IMAGE
    {
        return Err("stub requires a PE32+ executable".into());
    }
    Ok((header as usize, nt, directories))
}

/// Offset of the NT headers of a PE32+ executable (not a DLL).
pub fn pe_header(bytes: &[u8]) -> Result<usize, String> {
    pe_nt_headers(bytes).map(|(header, ..)| header)
}

/// Whether the PE image carries a certificate table (an Authenticode signature).
pub fn pe_is_signed(bytes: &[u8]) -> Result<bool, String> {
    let (_, _, directories) = pe_nt_headers(bytes)?;
    Ok(directories
        .get(pe::IMAGE_DIRECTORY_ENTRY_SECURITY)
        .is_some_and(|entry| entry.virtual_address.get(LE) != 0 || entry.size.get(LE) != 0))
}

fn take(bytes: &[u8], at: usize, len: usize) -> Result<&[u8], String> {
    bytes
        .get(at..at.checked_add(len).ok_or("executable offset overflow")?)
        .ok_or_else(|| "truncated executable".into())
}
fn word(bytes: &[u8], at: usize) -> Result<u32, String> {
    Ok(u32::from_le_bytes(take(bytes, at, 4)?.try_into().unwrap()))
}
fn short(bytes: &[u8], at: usize) -> Result<usize, String> {
    Ok(u16::from_le_bytes(take(bytes, at, 2)?.try_into().unwrap()) as usize)
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
    match FileKind::parse(bytes) {
        Ok(FileKind::Pe64) => {
            pe_header(bytes)?;
        }
        Ok(FileKind::Elf64) => {
            let header = elf_header(bytes)?;
            if !matches!(header.e_type(LE), elf::ET_EXEC | elf::ET_DYN) || header.e_entry(LE) == 0 {
                return Err("ELF stub is not an executable".into());
            }
        }
        Ok(FileKind::MachO64) => {
            macho_commands(bytes)?;
        }
        _ => {
            return Err(
                "unsupported standalone stub format; expected PE32+, ELF64 or thin Mach-O64".into(),
            )
        }
    }
    match object::File::parse(bytes).map_err(err)?.architecture() {
        Architecture::X86_64 => Ok(Arch::X86_64),
        Architecture::Aarch64 => Ok(Arch::Aarch64),
        _ => Err("unsupported standalone stub architecture".into()),
    }
}

fn elf_header(bytes: &[u8]) -> Result<&ElfHeader, String> {
    let header = ElfHeader::parse(bytes).map_err(err)?;
    header
        .endian()
        .map_err(|_| "stub requires little-endian ELF64".to_owned())?;
    Ok(header)
}

pub fn locate_elf_blob(bytes: &[u8]) -> Result<Option<Range<usize>>, String> {
    let header = elf_header(bytes)?;
    let sections = header.sections(LE, bytes).map_err(err)?;
    for section in sections.iter() {
        if sections.section_name(LE, section).map_err(err)? != b".note.lumen" {
            continue;
        }
        let note = section
            .notes(LE, bytes)
            .map_err(err)?
            .ok_or("invalid Lumen ELF note section")?
            .next()
            .map_err(err)?
            .filter(|note| {
                note.name() == b"LUMEN" && note.n_type(LE) == ELF_NOTE && note.desc().len() == 16
            })
            .ok_or("invalid Lumen ELF note")?;
        let desc = note.desc();
        let offset = usize::try_from(u64::from_le_bytes(desc[..8].try_into().unwrap()))
            .map_err(|_| "executable offset too large")?;
        let length = usize::try_from(u64::from_le_bytes(desc[8..].try_into().unwrap()))
            .map_err(|_| "executable offset too large")?;
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
    let header = elf_header(stub)?;
    let headers = header.section_headers(LE, stub).map_err(err)?;
    let names = header.shstrndx(LE, stub).map_err(err)? as usize;
    if header.e_shnum(LE) == 0 || header.e_shstrndx(LE) == elf::SHN_XINDEX || names >= headers.len()
    {
        return Err("ELF stub needs a nonextended section table".into());
    }
    if headers.len() > u16::MAX as usize - 2 {
        return Err("too many ELF sections".into());
    }
    let mut strings = headers[names].data(LE, stub).map_err(err)?.to_vec();
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
    let note_header = elf::NoteHeader64 {
        n_namesz: U32::new(LE, 6),
        n_descsz: U32::new(LE, 16),
        n_type: U32::new(LE, ELF_NOTE),
    };
    output.extend_from_slice(pod::bytes_of(&note_header));
    output.extend_from_slice(b"LUMEN\0\0\0");
    output.extend_from_slice(&(blob_offset as u64).to_le_bytes());
    output.extend_from_slice(&(blob.len() as u64).to_le_bytes());
    let note_len = output.len() - note_offset;
    let strings_offset = output.len();
    output.extend_from_slice(&strings);
    align(&mut output, 8)?;
    let new_headers = output.len();
    let mut table = headers.to_vec();
    table[names].sh_offset.set(LE, strings_offset as u64);
    table[names].sh_size.set(LE, strings.len() as u64);
    for (name, kind, offset, len, alignment) in [
        (
            blob_name,
            elf::SHT_PROGBITS,
            blob_offset,
            blob.len(),
            4096u64,
        ),
        (note_name, elf::SHT_NOTE, note_offset, note_len, 4),
    ] {
        table.push(elf::SectionHeader64 {
            sh_name: U32::new(LE, name),
            sh_type: U32::new(LE, kind),
            sh_flags: U64::new(LE, 0),
            sh_addr: U64::new(LE, 0),
            sh_offset: U64::new(LE, offset as u64),
            sh_size: U64::new(LE, len as u64),
            sh_link: U32::new(LE, 0),
            sh_info: U32::new(LE, 0),
            sh_addralign: U64::new(LE, alignment),
            sh_entsize: U64::new(LE, 0),
        });
    }
    output.extend_from_slice(pod::bytes_of_slice(&table));
    let (patched, _) =
        pod::from_bytes_mut::<ElfHeader>(&mut output).map_err(|()| "truncated executable")?;
    patched.e_shoff.set(LE, new_headers as u64);
    patched.e_shnum.set(LE, table.len() as u16);
    Ok(output)
}

type MachCommands<'a> = (&'a MachHeader, Vec<LoadCommandData<'a, LE>>);

fn macho_commands(bytes: &[u8]) -> Result<MachCommands<'_>, String> {
    const REQUIRED: &str = "stub requires a thin little-endian Mach-O64 executable";
    let header = MachHeader::parse(bytes, 0).map_err(err)?;
    header.endian().map_err(|_| REQUIRED.to_owned())?;
    if header.filetype(LE) != macho::MH_EXECUTE {
        return Err(REQUIRED.into());
    }
    let mut iter = header.load_commands(LE, bytes, 0).map_err(err)?;
    let mut commands = Vec::new();
    while let Some(command) = iter.next().map_err(err)? {
        commands.push(command);
    }
    Ok((header, commands))
}

pub fn locate_macho_blob(bytes: &[u8]) -> Result<Option<Range<usize>>, String> {
    for command in macho_commands(bytes)?.1 {
        let Some((segment, section_data)) = command.segment_64().map_err(err)? else {
            continue;
        };
        for section in segment.sections(LE, section_data).map_err(err)? {
            if section.name() == b"__blob" && section.segment_name() == b"__LUMEN" {
                let offset = section.offset(LE) as usize;
                let size =
                    usize::try_from(section.size(LE)).map_err(|_| "executable offset too large")?;
                take(bytes, offset, size)?;
                return Ok(Some(offset..offset + size));
            }
        }
    }
    Ok(None)
}

fn linkedit_segment<'a>(
    command: &LoadCommandData<'a, LE>,
) -> Result<Option<&'a macho::SegmentCommand64<LE>>, String> {
    Ok(command
        .segment_64()
        .map_err(err)?
        .map(|(segment, _)| segment)
        .filter(|segment| segment.name() == b"__LINKEDIT"))
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
    let (header, commands) = macho_commands(stub)?;
    if commands
        .iter()
        .any(|command| command.cmd() == macho::LC_DYLD_CHAINED_FIXUPS)
    {
        return Err("Mach-O chained-fixup stubs require relinking with classic dyld fixups before embedding".into());
    }
    let mut linkedit = None;
    for command in &commands {
        if let Some(segment) = linkedit_segment(command)? {
            linkedit = Some(segment);
        }
    }
    let linkedit = linkedit.ok_or("Mach-O stub needs a LINKEDIT segment")?;
    let old_offset =
        usize::try_from(linkedit.fileoff(LE)).map_err(|_| "executable offset too large")?;
    let old_len =
        usize::try_from(linkedit.filesize(LE)).map_err(|_| "executable offset too large")?;
    let old_vm = linkedit.vmaddr(LE);
    let linkedit_bytes = linkedit
        .data(LE, stub)
        .map_err(|()| "truncated executable")?;
    let page = if header.cputype(LE) == macho::CPU_TYPE_ARM64 {
        16384
    } else {
        4096
    };
    let mut first_data = old_offset;
    for command in &commands {
        if let Some((segment, section_data)) = command.segment_64().map_err(err)? {
            for section in segment.sections(LE, section_data).map_err(err)? {
                let offset = section.offset(LE) as usize;
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
        .checked_add(vm_len as u64)
        .ok_or("Mach-O VM range overflow")?;
    let delta = new_offset
        .checked_sub(old_offset)
        .ok_or("Mach-O LINKEDIT range overflow")?;
    let old_end = old_offset
        .checked_add(old_len)
        .ok_or("Mach-O LINKEDIT range overflow")?;
    let relocate = |field: &mut U32<LE>| -> Result<(), String> {
        let value = field.get(LE) as usize;
        if value == 0 {
            return Ok(());
        }
        if value < old_offset || value >= old_end {
            return Err("Mach-O table is outside LINKEDIT".into());
        }
        let updated = u32::try_from(value.checked_add(delta).ok_or("Mach-O table overflow")?)
            .map_err(|_| "Mach-O exceeds 4 GiB")?;
        field.set(LE, updated);
        Ok(())
    };
    let bad_command = |()| "invalid Mach-O load command".to_owned();
    let mut load_commands = Vec::new();
    let mut command_count = 0u32;
    for command in &commands {
        let kind = command.cmd();
        if kind == macho::LC_CODE_SIGNATURE {
            continue; // stale code signature; wrapper re-signs
        }
        let is_linkedit = linkedit_segment(command)?.is_some();
        if is_linkedit {
            let segment = macho::SegmentCommand64 {
                cmd: U32::new(LE, macho::LC_SEGMENT_64),
                cmdsize: U32::new(
                    LE,
                    (size_of::<macho::SegmentCommand64<LE>>() + size_of::<macho::Section64<LE>>())
                        as u32,
                ),
                segname: *b"__LUMEN\0\0\0\0\0\0\0\0\0",
                vmaddr: U64::new(LE, old_vm),
                vmsize: U64::new(LE, vm_len as u64),
                fileoff: U64::new(LE, blob_offset as u64),
                filesize: U64::new(LE, blob.len() as u64),
                maxprot: U32::new(LE, 1),
                initprot: U32::new(LE, 1),
                nsects: U32::new(LE, 1),
                flags: U32::new(LE, 0),
            };
            let section = macho::Section64 {
                sectname: *b"__blob\0\0\0\0\0\0\0\0\0\0",
                segname: *b"__LUMEN\0\0\0\0\0\0\0\0\0",
                addr: U64::new(LE, old_vm),
                size: U64::new(LE, blob.len() as u64),
                offset: U32::new(
                    LE,
                    u32::try_from(blob_offset).map_err(|_| "Mach-O exceeds 4 GiB")?,
                ),
                align: U32::new(LE, if page == 16384 { 14 } else { 12 }),
                reloff: U32::new(LE, 0),
                nreloc: U32::new(LE, 0),
                flags: U32::new(LE, 0),
                reserved1: U32::new(LE, 0),
                reserved2: U32::new(LE, 0),
                reserved3: U32::new(LE, 0),
            };
            load_commands.extend_from_slice(pod::bytes_of(&segment));
            load_commands.extend_from_slice(pod::bytes_of(&section));
            command_count += 1;
        }
        let mut raw = command.raw_data().to_vec();
        match kind {
            macho::LC_SYMTAB => {
                let (c, _) = pod::from_bytes_mut::<macho::SymtabCommand<LE>>(&mut raw)
                    .map_err(bad_command)?;
                relocate(&mut c.symoff)?;
                relocate(&mut c.stroff)?;
            }
            macho::LC_DYSYMTAB => {
                let (c, _) = pod::from_bytes_mut::<macho::DysymtabCommand<LE>>(&mut raw)
                    .map_err(bad_command)?;
                for field in [
                    &mut c.tocoff,
                    &mut c.modtaboff,
                    &mut c.extrefsymoff,
                    &mut c.indirectsymoff,
                    &mut c.extreloff,
                    &mut c.locreloff,
                ] {
                    relocate(field)?;
                }
            }
            macho::LC_DYLD_INFO | macho::LC_DYLD_INFO_ONLY => {
                let (c, _) = pod::from_bytes_mut::<macho::DyldInfoCommand<LE>>(&mut raw)
                    .map_err(bad_command)?;
                for field in [
                    &mut c.rebase_off,
                    &mut c.bind_off,
                    &mut c.weak_bind_off,
                    &mut c.lazy_bind_off,
                    &mut c.export_off,
                ] {
                    relocate(field)?;
                }
            }
            macho::LC_TWOLEVEL_HINTS => {
                let (c, _) = pod::from_bytes_mut::<macho::TwolevelHintsCommand<LE>>(&mut raw)
                    .map_err(bad_command)?;
                relocate(&mut c.offset)?;
            }
            macho::LC_SEGMENT_SPLIT_INFO
            | macho::LC_FUNCTION_STARTS
            | macho::LC_DATA_IN_CODE
            | macho::LC_DYLIB_CODE_SIGN_DRS
            | macho::LC_LINKER_OPTIMIZATION_HINT
            | macho::LC_DYLD_EXPORTS_TRIE => {
                let (c, _) = pod::from_bytes_mut::<macho::LinkeditDataCommand<LE>>(&mut raw)
                    .map_err(bad_command)?;
                relocate(&mut c.dataoff)?;
            }
            _ => {}
        }
        if is_linkedit {
            let (c, _) = pod::from_bytes_mut::<macho::SegmentCommand64<LE>>(&mut raw)
                .map_err(bad_command)?;
            if c.nsects.get(LE) != 0 {
                return Err("Mach-O LINKEDIT sections are unsupported".into());
            }
            c.vmaddr.set(LE, new_vm);
            c.fileoff.set(LE, new_offset as u64);
        }
        load_commands.extend(raw);
        command_count += 1;
    }
    let header_len = size_of::<MachHeader>();
    let end = header_len
        .checked_add(load_commands.len())
        .ok_or("Mach-O load command overflow")?;
    if end > first_data {
        return Err("Mach-O stub needs more header padding for __LUMEN".into());
    }
    let old_end = header_len + header.sizeofcmds(LE) as usize;
    let (patched, _) = pod::from_bytes_mut::<MachHeader>(&mut output).map_err(bad_command)?;
    patched.ncmds.set(LE, command_count);
    patched.sizeofcmds.set(
        LE,
        u32::try_from(load_commands.len()).map_err(|_| "Mach-O command table too large")?,
    );
    output[header_len..end.max(old_end)].fill(0);
    output[header_len..end].copy_from_slice(&load_commands);
    Ok(output)
}

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
        let offset = macho_commands(&packed)
            .unwrap()
            .1
            .iter()
            .find_map(|command| linkedit_segment(command).unwrap())
            .unwrap()
            .fileoff(LE) as usize;
        assert_eq!(&packed[offset..offset + 1024], &stub[1024..]);
        assert!(embed_macho(&packed, b"second").is_err());
        assert!(embed_macho(&stub[..1200], b"app").is_err());
    }
}
