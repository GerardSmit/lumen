//! Standalone image resources and process-lifetime read-only payload mapping.

use lumen_common::{executable, target::Arch};
use std::path::Path;

#[cfg(windows)]
mod windows {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    type Handle = *mut c_void;
    const NAME: &[u16] = &[76, 85, 77, 69, 78, 65, 79, 84, 0];
    const RCDATA: *const u16 = 10usize as *const u16;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetModuleHandleW(name: *const u16) -> Handle;
        fn FindResourceW(module: Handle, name: *const u16, kind: *const u16) -> Handle;
        fn LoadResource(module: Handle, resource: Handle) -> Handle;
        fn LockResource(resource: Handle) -> *const u8;
        fn SizeofResource(module: Handle, resource: Handle) -> u32;
        fn BeginUpdateResourceW(path: *const u16, delete_existing: i32) -> Handle;
        fn UpdateResourceW(update: Handle, kind: *const u16, name: *const u16, language: u16, data: *const u8, len: u32) -> i32;
        fn EndUpdateResourceW(update: Handle, discard: i32) -> i32;
        fn MoveFileExW(from: *const u16, to: *const u16, flags: u32) -> i32;
    }

    pub fn blob() -> Result<Option<&'static [u8]>, String> {
        // SAFETY: the primary executable module remains loaded for process lifetime;
        // Windows validates resource handles and returns an immutable mapped resource.
        unsafe {
            let module = GetModuleHandleW(std::ptr::null());
            if module.is_null() { return Err(std::io::Error::last_os_error().to_string()); }
            let resource = FindResourceW(module, NAME.as_ptr(), RCDATA);
            if resource.is_null() {
                let error = std::io::Error::last_os_error();
                return if matches!(error.raw_os_error(), Some(1812 | 1813 | 1814)) { Ok(None) } else { Err(error.to_string()) };
            }
            let len = SizeofResource(module, resource) as usize;
            let loaded = LoadResource(module, resource);
            let pointer = if loaded.is_null() { std::ptr::null() } else { LockResource(loaded) };
            if len == 0 || pointer.is_null() { return Err("invalid standalone RCDATA resource".into()); }
            Ok(Some(std::slice::from_raw_parts(pointer, len)))
        }
    }

    pub fn inject(path: &Path, blob: &[u8], icon: Option<&[u8]>) -> Result<(), String> {
        let icon = icon.map(super::executable::icon_resources).transpose()?;
        let path = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect::<Vec<_>>();
        let length = u32::try_from(blob.len()).map_err(|_| "standalone resource exceeds 4 GiB")?;
        // SAFETY: buffers remain alive through each synchronous Windows resource call;
        // Begin/End own the update handle and a failed update is discarded.
        unsafe {
            let update = BeginUpdateResourceW(path.as_ptr(), 0);
            if update.is_null() { return Err(std::io::Error::last_os_error().to_string()); }
            if UpdateResourceW(update, RCDATA, NAME.as_ptr(), 0, blob.as_ptr(), length) == 0 {
                let error = std::io::Error::last_os_error().to_string(); EndUpdateResourceW(update, 1); return Err(error);
            }
            if let Some((group, images)) = &icon {
                for (index, image) in images.iter().enumerate() {
                    if UpdateResourceW(update, 3usize as *const u16, (index + 1) as *const u16, 0,
                        image.as_ptr(), image.len() as u32) == 0 {
                        let error = std::io::Error::last_os_error().to_string(); EndUpdateResourceW(update, 1); return Err(error);
                    }
                }
                if UpdateResourceW(update, 14usize as *const u16, 1usize as *const u16, 0, group.as_ptr(), group.len() as u32) == 0 {
                    let error = std::io::Error::last_os_error().to_string(); EndUpdateResourceW(update, 1); return Err(error);
                }
            }
            if EndUpdateResourceW(update, 0) == 0 { return Err(std::io::Error::last_os_error().to_string()); }
        }
        Ok(())
    }

    pub fn publish(stage: &Path, output: &Path) -> Result<(), String> {
        let from = stage.as_os_str().encode_wide().chain(std::iter::once(0)).collect::<Vec<_>>();
        let to = output.as_os_str().encode_wide().chain(std::iter::once(0)).collect::<Vec<_>>();
        // SAFETY: both NUL-terminated path buffers remain valid for the synchronous call.
        if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 1 | 8) } == 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        Ok(())
    }
}

/// A successful mapping is retained until process exit, so deferred functions may
/// safely borrow the embedded bytes. A plain CLI releases its temporary mapping.
pub fn blob() -> Result<Option<&'static [u8]>, String> {
    #[cfg(windows)] { return windows::blob(); }
    #[cfg(any(target_os = "linux", target_os = "macos"))] {
        use std::os::fd::AsRawFd;
        let file = std::fs::File::open(std::env::current_exe().map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        let len = usize::try_from(file.metadata().map_err(|e| e.to_string())?.len()).map_err(|_| "standalone image too large")?;
        if len == 0 { return Err("empty standalone image".into()); }
        // SAFETY: the fd names a regular executable; mmap length is its checked
        // file length, PROT_READ is immutable, and no successful view is unmapped.
        let pointer = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_PRIVATE, file.as_raw_fd(), 0) };
        if pointer == libc::MAP_FAILED { return Err(std::io::Error::last_os_error().to_string()); }
        let bytes = unsafe { std::slice::from_raw_parts(pointer.cast::<u8>(), len) };
        let found = if bytes.starts_with(b"\x7fELF") { executable::locate_elf_blob(bytes) } else { executable::locate_macho_blob(bytes) };
        match found {
            Ok(Some(range)) => return Ok(Some(&bytes[range])),
            result => {
                unsafe { libc::munmap(pointer, len); }
                return result.map(|_| None);
            }
        }
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))] { Ok(None) }
}

/// Atomically replace an output with a successfully prepared standalone image.
pub fn publish(stage: &Path, output: &Path) -> Result<(), String> {
    #[cfg(windows)] { windows::publish(stage, output) }
    #[cfg(not(windows))] { std::fs::rename(stage, output).map_err(|e| format!("{}: {e}", output.display())) }
}

/// Publish an executable only after its resource/section and platform signing
/// step succeed. The supplied stub must implement this module's startup contract.
pub fn package(stub: &Path, output: &Path, blob: &[u8], arch: Arch, windows_gui: bool, icon: Option<&Path>) -> Result<usize, String> {
    use std::io::Write;
    if stub.canonicalize().map_err(|e| format!("{}: {e}", stub.display()))? == output.canonicalize().unwrap_or_default() {
        return Err("standalone output would overwrite its runtime stub".into());
    }
    let bytes = std::fs::read(stub).map_err(|e| format!("{}: {e}", stub.display()))?;
    if executable::architecture(&bytes)? != arch { return Err("standalone stub architecture differs from target".into()); }
    let pe = bytes.starts_with(b"MZ");
    let macho = bytes.starts_with(&0xfeed_facfu32.to_le_bytes());
    if pe && !cfg!(windows) { return Err("PE resource packaging requires a Windows host".into()); }
    if macho && !cfg!(target_os = "macos") { return Err("Mach-O packaging requires a macOS host for ad-hoc signing".into()); }
    if windows_gui && !pe { return Err("--windows-subsystem requires a Windows PE stub".into()); }
    if icon.is_some() && !pe { return Err("standalone --icon requires a Windows PE stub".into()); }
    let icon = icon.map(|path| std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))).transpose()?;
    if pe {
        let header = executable::pe_header(&bytes)?;
        if bytes[header + 24 + 144..header + 24 + 152].iter().any(|byte| *byte != 0) {
            return Err("signed PE stubs must be supplied unsigned and signed after app embedding".into());
        }
    }
    let packed = if pe { bytes } else if macho { executable::embed_macho(&bytes, blob)? } else { executable::embed_elf(&bytes, blob)? };
    if let Some(parent) = output.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let stage = output.with_extension(format!("stage-{}", std::process::id()));
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&stage)
        .map_err(|e| format!("{}: {e}", stage.display()))?;
    let result = (|| {
        file.write_all(&packed).map_err(|e| e.to_string())?; drop(file);
        #[cfg(windows)] if pe {
            windows::inject(&stage, blob, icon.as_deref())?;
            use std::io::{Seek, SeekFrom};
            let bytes = std::fs::read(&stage).map_err(|e| e.to_string())?;
            let header = executable::pe_header(&bytes)?;
            let mut file = std::fs::OpenOptions::new().write(true).open(&stage).map_err(|e| e.to_string())?;
            file.seek(SeekFrom::Start((header + 8) as u64)).map_err(|e| e.to_string())?;
            file.write_all(&0u32.to_le_bytes()).map_err(|e| e.to_string())?; // deterministic COFF timestamp
            file.seek(SeekFrom::Start((header + 24 + 64) as u64)).map_err(|e| e.to_string())?;
            file.write_all(&0u32.to_le_bytes()).map_err(|e| e.to_string())?; // optional image checksum
            file.seek(SeekFrom::Start((header + 24 + 68) as u64)).map_err(|e| e.to_string())?;
            file.write_all(&(if windows_gui {2u16} else {3}).to_le_bytes()).map_err(|e| e.to_string())?;
        }
        std::fs::set_permissions(&stage, std::fs::metadata(stub).map_err(|e| e.to_string())?.permissions()).map_err(|e| e.to_string())?;
        #[cfg(target_os = "macos")] if macho {
            let status = std::process::Command::new("codesign").args(["--force", "--sign", "-"]).arg(&stage)
                .arg("--identifier").arg(output.file_name().ok_or("standalone output needs a filename")?)
                .status().map_err(|e| format!("codesign: {e}"))?;
            if !status.success() { return Err("ad-hoc signing failed; standalone output was not published".into()); }
        }
        publish(&stage, output)?;
        usize::try_from(std::fs::metadata(output).map_err(|e| e.to_string())?.len()).map_err(|_| "standalone executable too large".into())
    })();
    if result.is_err() { let _ = std::fs::remove_file(&stage); }
    result
}
