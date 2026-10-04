//! Common prefix of the language-specific native data section.
//! Function ranges are code offsets and lengths; repeated/nonmonotonic offsets allow
//! identical functions to share code. The payload belongs to the language.

pub const MAGIC: &[u8; 8] = b"LUMDAT03";
pub const VERSION: u32 = 3;
const HEADER_LEN: usize = 28;
const FUNCTION_LEN: usize = 8;
const IMPORT_HEADER_LEN: usize = 12;

/// An empty `name` requests only the module; otherwise `signature_hash`
/// identifies the required native binding ABI within that module.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Import<'a> {
    pub module: &'a str,
    pub name: &'a str,
    pub signature_hash: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FunctionEntry {
    pub offset: u32,
    pub len: u32,
}

pub struct NativeData<'a> {
    pub functions: Vec<FunctionEntry>,
    pub imports: Vec<Import<'a>>,
    pub payload: &'a [u8],
}

impl<'a> NativeData<'a> {
    pub fn parse(bytes: &'a [u8], code_len: usize) -> Result<Self, &'static str> {
        if bytes.len() < HEADER_LEN || &bytes[..8] != MAGIC {
            return Err("invalid native data header");
        }
        let u32_at = |at| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
        if u32_at(8) != VERSION || u32_at(24) != 0 {
            return Err("unsupported native data version or flags");
        }
        let count = u32_at(12) as usize;
        let import_count = u32_at(16) as usize;
        let payload_len = u32_at(20) as usize;
        let table_end = count
            .checked_mul(FUNCTION_LEN)
            .and_then(|n| n.checked_add(HEADER_LEN))
            .ok_or("native function table too large")?;
        let imports_end = bytes
            .len()
            .checked_sub(payload_len)
            .ok_or("invalid native data length")?;
        if count == 0
            || table_end > imports_end
            || import_count > (imports_end - table_end) / IMPORT_HEADER_LEN
        {
            return Err("invalid native data length");
        }
        let mut functions = Vec::with_capacity(count);
        for record in bytes[HEADER_LEN..table_end].chunks_exact(FUNCTION_LEN) {
            let offset = u32::from_le_bytes(record[..4].try_into().unwrap());
            let len = u32::from_le_bytes(record[4..].try_into().unwrap());
            functions.push(FunctionEntry { offset, len });
        }
        validate_functions(&functions, code_len)?;
        let mut imports = Vec::with_capacity(import_count);
        let mut at = table_end;
        for _ in 0..import_count {
            let header_end = at
                .checked_add(IMPORT_HEADER_LEN)
                .filter(|&end| end <= imports_end)
                .ok_or("truncated native import")?;
            let module_len = u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap()) as usize;
            let name_len = u16::from_le_bytes(bytes[at + 2..at + 4].try_into().unwrap()) as usize;
            let signature_hash = u64::from_le_bytes(bytes[at + 4..header_end].try_into().unwrap());
            at = header_end;
            let module_end = at
                .checked_add(module_len)
                .filter(|&end| end <= imports_end)
                .ok_or("truncated native import")?;
            let name_end = module_end
                .checked_add(name_len)
                .filter(|&end| end <= imports_end)
                .ok_or("truncated native import")?;
            let module = std::str::from_utf8(&bytes[at..module_end])
                .map_err(|_| "invalid native module name")?;
            let name = std::str::from_utf8(&bytes[module_end..name_end])
                .map_err(|_| "invalid native binding name")?;
            let import = Import {
                module,
                name,
                signature_hash,
            };
            validate_import(import)?;
            if imports.last().is_some_and(|previous: &Import<'_>| {
                (previous.module, previous.name) >= (module, name)
            }) {
                return Err("native imports out of order");
            }
            imports.push(import);
            at = name_end;
        }
        if at != imports_end {
            return Err("native data has trailing import bytes");
        }
        Ok(Self {
            functions,
            imports,
            payload: &bytes[imports_end..],
        })
    }
}

pub fn encode(
    functions: &[FunctionEntry],
    payload: &[u8],
    code_len: usize,
) -> Result<Vec<u8>, &'static str> {
    encode_with_imports(functions, &[], payload, code_len)
}

pub fn encode_with_imports(
    functions: &[FunctionEntry],
    imports: &[Import<'_>],
    payload: &[u8],
    code_len: usize,
) -> Result<Vec<u8>, &'static str> {
    let count = u32::try_from(functions.len()).map_err(|_| "too many native functions")?;
    let import_count = u32::try_from(imports.len()).map_err(|_| "too many native imports")?;
    let payload_len = u32::try_from(payload.len()).map_err(|_| "native payload too large")?;
    validate_functions(functions, code_len)?;
    let mut capacity = functions
        .len()
        .checked_mul(FUNCTION_LEN)
        .and_then(|n| n.checked_add(HEADER_LEN))
        .and_then(|n| n.checked_add(payload.len()))
        .ok_or("native data too large")?;
    for (i, &import) in imports.iter().enumerate() {
        validate_import(import)?;
        if i > 0 && (imports[i - 1].module, imports[i - 1].name) >= (import.module, import.name) {
            return Err("native imports out of order");
        }
        capacity = capacity
            .checked_add(IMPORT_HEADER_LEN)
            .and_then(|n| n.checked_add(import.module.len()))
            .and_then(|n| n.checked_add(import.name.len()))
            .ok_or("native data too large")?;
    }
    let mut bytes = Vec::with_capacity(capacity);
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&VERSION.to_le_bytes());
    bytes.extend_from_slice(&count.to_le_bytes());
    bytes.extend_from_slice(&import_count.to_le_bytes());
    bytes.extend_from_slice(&payload_len.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    for entry in functions {
        bytes.extend_from_slice(&entry.offset.to_le_bytes());
        bytes.extend_from_slice(&entry.len.to_le_bytes());
    }
    for import in imports {
        bytes.extend_from_slice(&(import.module.len() as u16).to_le_bytes());
        bytes.extend_from_slice(&(import.name.len() as u16).to_le_bytes());
        bytes.extend_from_slice(&import.signature_hash.to_le_bytes());
        bytes.extend_from_slice(import.module.as_bytes());
        bytes.extend_from_slice(import.name.as_bytes());
    }
    bytes.extend_from_slice(payload);
    Ok(bytes)
}

fn validate_functions(functions: &[FunctionEntry], code_len: usize) -> Result<(), &'static str> {
    if functions.is_empty()
        || functions.iter().any(|entry| {
            entry.len == 0
                || entry.offset % 4 != 0
                || (entry.offset as usize)
                    .checked_add(entry.len as usize)
                    .is_none_or(|end| end > code_len)
        })
    {
        return Err("native function range out of code section");
    }
    let mut sorted = functions.to_vec();
    sorted.sort_unstable_by_key(|entry| (entry.offset, entry.len));
    sorted.dedup();
    if sorted
        .windows(2)
        .any(|pair| pair[0].offset as u64 + pair[0].len as u64 > pair[1].offset as u64)
    {
        return Err("native function ranges overlap");
    }
    Ok(())
}

fn validate_import(import: Import<'_>) -> Result<(), &'static str> {
    if import.module.is_empty()
        || import.module.len() > u16::MAX as usize
        || import.name.len() > u16::MAX as usize
        || (import.name.is_empty() && import.signature_hash != 0)
        || (!import.name.is_empty() && import.signature_hash == 0)
        || import.module.bytes().any(|b| b == 0)
        || import.name.bytes().any(|b| b == 0)
    {
        return Err("invalid native import");
    }
    Ok(())
}
