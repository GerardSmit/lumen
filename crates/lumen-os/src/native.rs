//! Language-neutral native image loading. The embedder supplies page mapping and
//! signature policy; no compiler or bytecode VM is required here.

use lumen_common::aot::{
    got, native_data::FunctionEntry, native_lines, NativeContainer, SEC_NATIVE_CODE,
    SEC_NATIVE_DATA, SEC_NATIVE_LINES,
};
use lumen_common::target::{Arch, Placement, TargetSpec};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};
mod locations;
mod unwind;

/// Optional platform unwinder for embedders. Registration must retain or copy
/// the recipes, and return a token released before the code mapping is freed.
#[derive(Clone, Copy)]
pub struct NativeUnwindBackend {
    pub register: unsafe fn(
        *const u8,
        Arch,
        &[FunctionEntry],
        &[lumen_common::aot::native_unwind::Record],
    ) -> Result<usize, String>,
    pub unregister: unsafe fn(usize),
}

static UNWIND_BACKEND: OnceLock<NativeUnwindBackend> = OnceLock::new();

pub unsafe fn install_native_unwind_backend(backend: NativeUnwindBackend) -> bool {
    UNWIND_BACKEND.set(backend).is_ok()
}

/// The returned mapping must be page aligned, RW/NX, and at least `len` bytes.
/// `seal_code` cleans the data cache, invalidates the instruction cache, then
/// changes only the first `code_len` bytes of the `mapped_len` allocation to
/// RX. GOT pages stay RW/NX.
/// For XIP, `map_xip` must pin immutable validated storage independently of
/// the caller's byte slice, map identical code bytes RX at `base`, map following
/// GOT pages RW/NX, and return one contiguous virtual range. `free` releases
/// either kind of mapping; XIP code pages themselves are never modified.
/// A null result owns no resources. RAM-placement targets may try this callback
/// for page-aligned code and fall back to copying; explicit XIP requires success.
#[derive(Clone, Copy)]
pub struct NativeBackend {
    pub page_size: usize,
    pub alloc: unsafe extern "C" fn(len: usize) -> *mut u8,
    pub seal_code: unsafe extern "C" fn(base: *mut u8, mapped_len: usize, code_len: usize) -> bool,
    pub map_xip: Option<
        unsafe extern "C" fn(
            blob: *const u8,
            blob_len: usize,
            code_offset: usize,
            code_len: usize,
            got_pages: usize,
        ) -> *mut u8,
    >,
    pub free: unsafe extern "C" fn(base: *mut u8, len: usize),
}

static BACKEND: OnceLock<NativeBackend> = OnceLock::new();

/// Install before loading an image. The embedder owns mapping correctness and
/// must keep its callbacks available until all images have been dropped.
/// Allocation, sealing and release callbacks must be safe on any loader thread.
pub unsafe fn install_native_backend(backend: NativeBackend) -> bool {
    BACKEND.set(backend).is_ok()
}

/// Install the desktop W^X page backend. Bare-metal embedders install their
/// own backend instead.
///
#[cfg(any(windows, unix))]
pub fn install_host_backend() -> bool {
    let page_size = crate::jitmem::host_page_size() as usize;
    if page_size == 0 || !page_size.is_power_of_two() {
        return false;
    }
    unsafe extern "C" fn alloc(len: usize) -> *mut u8 {
        crate::jitmem::alloc_native_image(len)
    }
    unsafe extern "C" fn seal_code(base: *mut u8, mapped_len: usize, code_len: usize) -> bool {
        #[cfg(target_os = "macos")]
        {
            crate::jitmem::seal_native_image(base, mapped_len, code_len)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = mapped_len;
            crate::jitmem::seal_native_code(base, code_len)
        }
    }
    unsafe extern "C" fn free(base: *mut u8, len: usize) {
        crate::jitmem::free_native_image(base, len)
    }
    unsafe {
        install_native_backend(NativeBackend {
            page_size,
            alloc,
            seal_code,
            map_xip: None,
            free,
        })
    }
}

pub struct LoadedNative {
    base: *mut u8,
    mapped_len: usize,
    code_len: usize,
    got_len: usize,
    data: Vec<u8>,
    functions: Vec<FunctionEntry>,
    lines: Option<OwnedLines>,
    blob_hash: [u8; 32],
    backend: Option<NativeBackend>,
    got_base: *mut usize,
    linked: bool,
    xip: bool,
    unwind: Option<unwind::Registration>,
    locations: Option<locations::Registration>,
}

// Mapping callbacks must support release from any thread. Published images have
// immutable Rust metadata; mutation through their raw GOT pointer is unsafe.
unsafe impl Send for LoadedNative {}
unsafe impl Sync for LoadedNative {}

struct OwnedLines {
    files: Vec<String>,
    locations: Vec<native_lines::Location>,
}

/// Resolve a PC in any live native image without allocation or locks.
/// Images publish immutable metadata before execution and retire it on drop.
pub fn location_at(pc: usize) -> Option<NativeLocation> {
    locations::location_at(pc)
}

/// A native program counter expressed without a device-specific address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeLocation {
    pub blob_hash: [u8; 32],
    pub function: u32,
    pub code_offset: u32,
}

impl LoadedNative {
    pub fn code(&self) -> *const u8 {
        self.base
    }

    pub fn code_len(&self) -> usize {
        self.code_len
    }

    pub fn got(&self) -> *const usize {
        self.got_base
    }

    pub fn got_len(&self) -> usize {
        self.got_len
    }

    pub fn data(&self) -> &[u8] {
        &self.data
    }

    pub fn entry(&self, index: usize) -> Option<*const u8> {
        self.functions
            .get(index)
            .map(|entry| unsafe { self.base.add(entry.offset as usize) } as *const u8)
    }

    pub fn entry_count(&self) -> usize {
        self.functions.len()
    }

    pub fn blob_hash(&self) -> [u8; 32] {
        self.blob_hash
    }

    /// Map a faulting instruction pointer within a function's exact code range.
    /// Identical folded functions share an address; the lowest index names it.
    pub fn location_at(&self, pc: usize) -> Option<NativeLocation> {
        let offset = pc.checked_sub(self.base as usize)?;
        if offset >= self.code_len {
            return None;
        }
        let (function, entry) = self.functions.iter().enumerate().find(|(_, entry)| {
            offset >= entry.offset as usize && offset < entry.offset as usize + entry.len as usize
        })?;
        Some(NativeLocation {
            blob_hash: self.blob_hash,
            function: u32::try_from(function).ok()?,
            code_offset: u32::try_from(offset - entry.offset as usize).ok()?,
        })
    }

    /// Resolve an embedded line table without allocation in the fault path.
    pub fn source_at(&self, pc: usize) -> Option<(&str, u32, u32)> {
        let location = self.location_at(pc)?;
        let lines = self.lines.as_ref()?;
        let at = lines.locations.partition_point(|entry| {
            (entry.function, entry.code_offset) <= (location.function, location.code_offset)
        });
        let entry = lines.locations.get(at.checked_sub(1)?)?;
        (entry.function == location.function)
            .then_some(entry)
            .and_then(|entry| {
                lines
                    .files
                    .get(entry.file as usize)
                    .map(|file| (file.as_str(), entry.line, entry.column))
            })
    }

    pub fn is_linked(&self) -> bool {
        self.linked
    }

    pub fn is_xip(&self) -> bool {
        self.xip
    }
}

impl Drop for LoadedNative {
    fn drop(&mut self) {
        drop(self.locations.take());
        drop(self.unwind.take());
        if let Some(backend) = self.backend {
            unsafe { (backend.free)(self.base, self.mapped_len) }
        }
    }
}

/// Verify policy and image metadata before allocation, then resolve the whole
/// GOT before publishing RX code. Function slots resolve from the shared function
/// table; `resolve` handles helpers, statics, atoms, IC cells and imports.
pub fn load(
    bytes: &[u8],
    target: &TargetSpec,
    verify_signature: impl FnOnce(&[u8]) -> Result<(), String>,
    resolve_import: impl FnMut(&str, &str, u64) -> Option<usize>,
    resolve: impl FnMut(got::Kind, u32, *const u8, &[u8]) -> Option<usize>,
) -> Result<LoadedNative, String> {
    load_impl(
        bytes,
        target,
        verify_signature,
        resolve_import,
        resolve,
        None,
    )
}

type CacheKey = ([u8; 32], Vec<u8>);
static IMAGE_CACHE: OnceLock<Mutex<BTreeMap<CacheKey, Weak<LoadedNative>>>> = OnceLock::new();

/// Share mappings whose GOT has no realm-specific addresses. Authorization,
/// full container validation and helper resolution still run on every load.
pub fn load_shared(
    bytes: &[u8],
    target: &TargetSpec,
    verify_signature: impl FnOnce(&[u8]) -> Result<(), String>,
    mut resolve_import: impl FnMut(&str, &str, u64) -> Option<usize>,
    mut resolve: impl FnMut(got::Kind, u32, *const u8, &[u8]) -> Option<usize>,
) -> Result<Arc<LoadedNative>, String> {
    verify_signature(bytes)?;
    let container = NativeContainer::parse(bytes).map_err(str::to_owned)?;
    container.mapping_len(target).map_err(str::to_owned)?;
    if Some(target.arch) != host_arch() || target.pointer_width as u32 != usize::BITS {
        return Err("native image architecture or pointer width differs from loader".into());
    }
    let shareable = container
        .got_relocs
        .iter()
        .all(|slot| matches!(slot.kind, got::Kind::Helper | got::Kind::Function));
    if !shareable {
        return load(bytes, target, |_| Ok(()), resolve_import, resolve).map(Arc::new);
    }
    for import in &container.required_imports {
        let address = resolve_import(import.module, import.name, import.signature_hash)
            .ok_or_else(|| format!("missing native import {}::{}", import.module, import.name))?;
        if !import.name.is_empty() && address == 0 {
            return Err(format!(
                "missing native import {}::{}",
                import.module, import.name
            ));
        }
    }
    let key = (
        lumen_common::aot::sidecar::hash(bytes),
        target.encode().map_err(str::to_owned)?.to_vec(),
    );
    let cache = IMAGE_CACHE.get_or_init(|| Mutex::new(BTreeMap::new()));
    let cached = cache
        .lock()
        .map_err(|_| "native image cache poisoned")?
        .get(&key)
        .and_then(Weak::upgrade);
    if let Some(image) = cached {
        for reloc in &container.got_relocs {
            if reloc.kind == got::Kind::Helper {
                let address = resolve(reloc.kind, reloc.index, image.code(), image.data())
                    .ok_or("missing shared native helper")?;
                if address != unsafe { *image.got().add(reloc.slot as usize) } {
                    return Err("shared native helper differs between realms".into());
                }
            }
        }
        return Ok(image);
    }
    let image = Arc::new(load(bytes, target, |_| Ok(()), resolve_import, resolve)?);
    let mut entries = cache.lock().map_err(|_| "native image cache poisoned")?;
    if let Some(existing) = entries.get(&key).and_then(Weak::upgrade) {
        for reloc in &container.got_relocs {
            if reloc.kind == got::Kind::Helper
                && unsafe { *existing.got().add(reloc.slot as usize) }
                    != unsafe { *image.got().add(reloc.slot as usize) }
            {
                return Err("shared native helper differs between realms".into());
            }
        }
        drop(entries);
        return Ok(existing);
    }
    entries.retain(|_, image| image.strong_count() != 0);
    entries.insert(key, Arc::downgrade(&image));
    Ok(image)
}

/// Use native code and a GOT placed by the system linker. The caller pins both
/// ranges until every returned image is dropped and serializes GOT initialization.
/// Code must implement this blob's functions, including all linker relocations.
/// Existing nonzero slots must resolve identically in every realm sharing the GOT.
/// Static unwind sections are registered by the system loader.
pub unsafe fn load_linked(
    bytes: &[u8],
    target: &TargetSpec,
    code: *const u8,
    code_len: usize,
    got: *mut usize,
    got_len: usize,
    verify_signature: impl FnOnce(&[u8]) -> Result<(), String>,
    resolve_import: impl FnMut(&str, &str, u64) -> Option<usize>,
    resolve: impl FnMut(got::Kind, u32, *const u8, &[u8]) -> Option<usize>,
) -> Result<LoadedNative, String> {
    if code.is_null() || got.is_null() || (got as usize) % std::mem::align_of::<usize>() != 0 {
        return Err("invalid linked native ranges".into());
    }
    load_impl(
        bytes,
        target,
        verify_signature,
        resolve_import,
        resolve,
        Some((code, code_len, got, got_len)),
    )
}

/// Full allocation for a dynamically mapped image, including platform unwind
/// storage. Linked images use caller-owned ranges and do not allocate this tail.
/// XIP backends receive this same length split into code and writable pages.
pub fn required_mapping_len(bytes: &[u8], target: &TargetSpec) -> Result<usize, String> {
    let container = NativeContainer::parse(bytes).map_err(str::to_owned)?;
    mapping_len_with_unwind(&container, target)
}

fn mapping_len_with_unwind(
    container: &NativeContainer<'_>,
    target: &TargetSpec,
) -> Result<usize, String> {
    let len = container.mapping_len(target).map_err(str::to_owned)?;
    let records = container
        .sections
        .iter()
        .find(|section| section.kind == lumen_common::aot::SEC_NATIVE_UNWIND)
        .map(|section| lumen_common::aot::native_unwind::decode(section.data, &container.functions))
        .transpose()
        .map_err(str::to_owned)?
        .unwrap_or_default();
    let tail = unwind::storage_len(target.arch, &records)?;
    let page = target.page_size as usize;
    len.checked_add(tail)
        .and_then(|len| len.checked_add(page - 1))
        .map(|len| len & !(page - 1))
        .ok_or_else(|| "native unwind mapping overflow".into())
}

fn load_impl(
    bytes: &[u8],
    target: &TargetSpec,
    verify_signature: impl FnOnce(&[u8]) -> Result<(), String>,
    mut resolve_import: impl FnMut(&str, &str, u64) -> Option<usize>,
    mut resolve: impl FnMut(got::Kind, u32, *const u8, &[u8]) -> Option<usize>,
    linked: Option<(*const u8, usize, *mut usize, usize)>,
) -> Result<LoadedNative, String> {
    verify_signature(bytes)?;
    let container = NativeContainer::parse(bytes).map_err(str::to_owned)?;
    let mut mapped_len = container.mapping_len(target).map_err(str::to_owned)?;
    if Some(target.arch) != host_arch() {
        return Err("native image architecture differs from loader".into());
    }
    if target.pointer_width as u32 != usize::BITS {
        return Err("native image pointer width differs from loader".into());
    }
    let page = target.page_size as usize;
    let section = |kind| {
        container
            .sections
            .iter()
            .find(|s| s.kind == kind)
            .map(|s| s.data)
            .unwrap()
    };
    let code = section(SEC_NATIVE_CODE);
    let data = section(SEC_NATIVE_DATA);
    let relocs = &container.got_relocs;
    let functions = &container.functions;
    let lines = container
        .sections
        .iter()
        .find(|section| section.kind == SEC_NATIVE_LINES)
        .map(|section| {
            let lines = native_lines::decode(section.data, functions)?;
            Ok::<_, &'static str>(OwnedLines {
                files: lines.files.into_iter().map(str::to_owned).collect(),
                locations: lines.locations,
            })
        })
        .transpose()
        .map_err(str::to_owned)?;
    let mut import_addresses = Vec::with_capacity(container.required_imports.len());
    for import in &container.required_imports {
        let address = resolve_import(import.module, import.name, import.signature_hash)
            .ok_or_else(|| format!("missing native import {}::{}", import.module, import.name))?;
        if !import.name.is_empty() && address == 0 {
            return Err(format!(
                "missing native import {}::{}",
                import.module, import.name
            ));
        }
        import_addresses.push(address);
    }
    let unwind_records = container
        .sections
        .iter()
        .find(|section| section.kind == lumen_common::aot::SEC_NATIVE_UNWIND)
        .map(|section| lumen_common::aot::native_unwind::decode(section.data, functions))
        .transpose()
        .map_err(str::to_owned)?
        .unwrap_or_default();
    let unwind_offset = mapped_len;
    if linked.is_none() {
        mapped_len = mapping_len_with_unwind(&container, target)?;
    }
    let got_pages = mapped_len - code.len();
    let requires_xip = matches!(target.code_placement, Placement::ExecuteInPlace { .. });
    let mut xip = false;
    let backend = if linked.is_some() {
        None
    } else {
        Some(*BACKEND.get().ok_or("native mapping backend unavailable")?)
    };
    let base = if let Some((code_ptr, code_len, _, got_len)) = linked {
        if code_len != code.len() || got_len != relocs.len() {
            return Err("linked native range lengths differ from metadata".into());
        }
        code_ptr as *mut u8
    } else {
        let backend = backend.unwrap();
        if backend.page_size != page {
            return Err("native target page size differs from mapping backend".into());
        }
        let mapped = if requires_xip || container.code_offset() % page == 0 {
            backend.map_xip.map_or(std::ptr::null_mut(), |map| unsafe {
                map(
                    bytes.as_ptr(),
                    bytes.len(),
                    container.code_offset(),
                    code.len(),
                    got_pages,
                )
            })
        } else {
            std::ptr::null_mut()
        };
        if !mapped.is_null() {
            xip = true;
            mapped
        } else if requires_xip {
            return Err("required native XIP mapping unavailable".into());
        } else {
            unsafe { (backend.alloc)(mapped_len) }
        }
    };
    if base.is_null() {
        return Err("cannot allocate native image mapping".into());
    }
    let mut image = LoadedNative {
        base,
        mapped_len,
        code_len: code.len(),
        got_len: relocs.len(),
        data: data.to_vec(),
        functions: functions.clone(),
        lines,
        blob_hash: lumen_common::aot::sidecar::hash(bytes),
        backend,
        got_base: linked
            .map(|(_, _, got, _)| got)
            .unwrap_or_else(|| unsafe { base.add(code.len()) }.cast()),
        linked: linked.is_some(),
        xip,
        unwind: None,
        locations: None,
    };
    if linked.is_none() && (base as usize) % page != 0 {
        return Err("native mapping is not page aligned".into());
    }
    if linked.is_some() {
        // System linker owns executable pages.
    } else if xip {
        if unsafe { std::slice::from_raw_parts(base, code.len()) } != code {
            return Err("XIP mapping differs from signed native code".into());
        }
    } else {
        unsafe { std::ptr::copy_nonoverlapping(code.as_ptr(), base, code.len()) };
    }
    let got_ptr = image.got_base;
    if linked.is_none() {
        unsafe { std::ptr::write_bytes(got_ptr.cast::<u8>(), 0, got_pages) };
    }
    let mut resolved_got = vec![0; relocs.len()];
    let got = if linked.is_some() {
        resolved_got.as_mut_slice()
    } else {
        unsafe { std::slice::from_raw_parts_mut(got_ptr, relocs.len()) }
    };
    got::fill(got, relocs, |kind, index| {
        if kind == got::Kind::Function {
            Some(unsafe { base.add(functions[index as usize].offset as usize) } as usize)
        } else if kind == got::Kind::NativeImport {
            import_addresses.get(index as usize).copied()
        } else {
            resolve(kind, index, base.cast(), &image.data)
        }
    })
    .map_err(str::to_owned)?;
    if linked.is_some() {
        let actual = unsafe { std::slice::from_raw_parts_mut(got_ptr, relocs.len()) };
        if actual
            .iter()
            .zip(&resolved_got)
            .any(|(&old, &new)| old != 0 && old != new)
        {
            return Err("linked native GOT differs between realms".into());
        }
        for (slot, value) in actual.iter_mut().zip(resolved_got) {
            if *slot == 0 {
                *slot = value;
            }
        }
    }
    if linked.is_none()
        && !xip
        && !unsafe { (backend.unwrap().seal_code)(base, mapped_len, code.len()) }
    {
        return Err("cannot seal native code pages".into());
    }
    if linked.is_none() && !unwind_records.is_empty() {
        image.unwind = Some(unsafe {
            unwind::Registration::new(base, unwind_offset, target.arch, functions, &unwind_records)?
        });
    }
    image.locations = Some(locations::Registration::new(
        base as usize,
        code.len(),
        image.blob_hash,
        functions,
    ));
    Ok(image)
}

/// Release-policy entry point: a detached signature must match an allowed key.
#[cfg(feature = "native-signing")]
pub fn load_signed(
    bytes: &[u8],
    target: &TargetSpec,
    signature: &[u8; lumen_common::aot::signature::SIGNATURE_LEN],
    allowed_keys: &[[u8; lumen_common::aot::signature::PUBLIC_KEY_LEN]],
    resolve_import: impl FnMut(&str, &str, u64) -> Option<usize>,
    resolve: impl FnMut(got::Kind, u32, *const u8, &[u8]) -> Option<usize>,
) -> Result<LoadedNative, String> {
    load(
        bytes,
        target,
        |blob| {
            lumen_common::aot::signature::verify(blob, signature, allowed_keys)
                .map_err(str::to_owned)
        },
        resolve_import,
        resolve,
    )
}

fn host_arch() -> Option<Arch> {
    #[cfg(target_arch = "aarch64")]
    {
        return Some(Arch::Aarch64);
    }
    #[cfg(target_arch = "x86_64")]
    {
        return Some(Arch::X86_64);
    }
    #[allow(unreachable_code)]
    None
}
