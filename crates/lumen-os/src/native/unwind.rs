use lumen_common::aot::{
    native_data::FunctionEntry,
    native_unwind::{self, Record},
};
use lumen_common::target::Arch;

pub(super) struct Registration {
    custom: Option<(super::NativeUnwindBackend, usize)>,
    registered: bool,
    #[cfg(unix)]
    frames: Vec<u8>,
    #[cfg(windows)]
    table: Vec<RuntimeFunction>,
}

#[cfg(windows)]
#[repr(C)]
struct RuntimeFunction {
    begin: u32,
    #[cfg(not(target_arch = "aarch64"))]
    end: u32,
    unwind: u32,
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn RtlAddFunctionTable(table: *mut RuntimeFunction, count: u32, base: u64) -> u8;
    fn RtlDeleteFunctionTable(table: *mut RuntimeFunction) -> u8;
}

#[cfg(unix)]
unsafe extern "C" {
    fn __register_frame(frame: *const u8);
    fn __deregister_frame(frame: *const u8);
}

pub(super) fn storage_len(arch: Arch, records: &[Record]) -> Result<usize, String> {
    #[cfg(windows)]
    {
        if !matches!(arch, Arch::X86_64 | Arch::Aarch64) && !records.is_empty() {
            return Err("Windows unwind architecture unavailable".into());
        }
        if records.iter().any(|record| record.windows.is_empty()) {
            return Err("missing Windows unwind recipe".into());
        }
        return records.iter().try_fold(0usize, |sum, record| {
            sum.checked_add(record.windows.len())
                .ok_or_else(|| "unwind storage overflow".into())
        });
    }
    #[cfg(not(windows))]
    {
        let _ = (arch, records);
        Ok(0)
    }
}

impl Registration {
    pub(super) unsafe fn new(
        base: *mut u8,
        storage_offset: usize,
        arch: Arch,
        functions: &[FunctionEntry],
        records: &[Record],
    ) -> Result<Self, String> {
        if let Some(backend) = super::UNWIND_BACKEND.get() {
            let token = (backend.register)(base, arch, functions, records)?;
            return Ok(Self {
                custom: Some((*backend, token)),
                registered: false,
                #[cfg(unix)]
                frames: Vec::new(),
                #[cfg(windows)]
                table: Vec::new(),
            });
        }
        #[cfg(unix)]
        {
            let _ = storage_offset;
            let (mut frames, relocations) =
                native_unwind::eh_frame(arch, functions, records).map_err(str::to_owned)?;
            for (at, function) in relocations {
                let address = base.add(functions[function as usize].offset as usize) as u64;
                frames[at..at + 8].copy_from_slice(&address.to_le_bytes());
            }
            let result = Self {
                frames,
                custom: None,
                registered: true,
            };
            #[cfg(not(target_os = "macos"))]
            __register_frame(result.frames.as_ptr());
            #[cfg(target_os = "macos")]
            result.for_each_fde(|fde| __register_frame(fde));
            Ok(result)
        }
        #[cfg(windows)]
        {
            let mut offset = storage_offset;
            let mut table = Vec::new();
            for record in records {
                let function = &functions[record.function as usize];
                native_unwind::validate_windows(arch, &record.windows, function.len)
                    .map_err(str::to_owned)?;
                if table
                    .iter()
                    .any(|entry: &RuntimeFunction| entry.begin == function.offset)
                {
                    continue;
                }
                let unwind = u32::try_from(offset).map_err(|_| "Windows unwind RVA overflow")?;
                std::ptr::copy_nonoverlapping(
                    record.windows.as_ptr(),
                    base.add(offset),
                    record.windows.len(),
                );
                offset += record.windows.len();
                table.push(RuntimeFunction {
                    begin: function.offset,
                    #[cfg(not(target_arch = "aarch64"))]
                    end: function
                        .offset
                        .checked_add(function.len)
                        .ok_or("Windows function range overflow")?,
                    unwind,
                });
            }
            table.sort_by_key(|entry| entry.begin);
            if RtlAddFunctionTable(
                table.as_mut_ptr(),
                u32::try_from(table.len()).map_err(|_| "too many Windows unwind functions")?,
                base as u64,
            ) == 0
            {
                return Err("cannot register Windows native unwind table".into());
            }
            Ok(Self {
                table,
                custom: None,
                registered: true,
            })
        }
        #[cfg(not(any(windows, unix)))]
        {
            let _ = (base, storage_offset, arch, functions, records);
            Ok(Self {
                custom: None,
                registered: false,
            })
        }
    }

    #[cfg(target_os = "macos")]
    unsafe fn for_each_fde(&self, mut visit: impl FnMut(*const u8)) {
        let mut at = 0;
        while at + 4 <= self.frames.len() {
            let len = u32::from_le_bytes(self.frames[at..at + 4].try_into().unwrap()) as usize;
            if len == 0 {
                break;
            }
            if at != 0 {
                visit(self.frames.as_ptr().add(at));
            }
            at += 4 + len;
        }
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        unsafe {
            if let Some((backend, token)) = self.custom.take() {
                (backend.unregister)(token);
            }
            if !self.registered {
                return;
            }
            #[cfg(all(unix, not(target_os = "macos")))]
            __deregister_frame(self.frames.as_ptr());
            #[cfg(target_os = "macos")]
            self.for_each_fde(|fde| __deregister_frame(fde));
            #[cfg(windows)]
            {
                RtlDeleteFunctionTable(self.table.as_mut_ptr());
            }
        }
    }
}
