//! Executable memory with a W^X policy: code is written into read-write pages, which are then
//! made read-execute (macOS uses `MAP_JIT` with the per-thread write-protect toggle).

/// A block of executable code, freed on drop.
pub struct ExecMemory {
    ptr: *mut u8,
    len: usize,
}

impl ExecMemory {
    /// Copy `code` into fresh executable memory.
    pub fn new(code: &[u8]) -> Result<ExecMemory, String> {
        ExecMemory::with_len(code.len(), |buf, _| buf[..code.len()].copy_from_slice(code))
    }

    /// Allocate `len` bytes, let `fill` write them (it also gets the final base address, for
    /// absolute relocations), then make them executable.
    pub fn with_len(len: usize, fill: impl FnOnce(&mut [u8], u64)) -> Result<ExecMemory, String> {
        let len = len.max(1);
        let ptr = unsafe { sys::alloc(len) };
        if ptr.is_null() {
            return Err("jit: cannot allocate executable memory".into());
        }
        fill(unsafe { std::slice::from_raw_parts_mut(ptr, len) }, ptr as u64);
        if !unsafe { sys::make_exec(ptr, len) } {
            unsafe { sys::free_exec(ptr, len) };
            return Err("jit: cannot make memory executable".into());
        }
        Ok(ExecMemory { ptr, len })
    }

    pub fn as_ptr(&self) -> *const u8 {
        self.ptr
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Drop for ExecMemory {
    fn drop(&mut self) {
        unsafe { sys::free_exec(self.ptr, self.len) }
    }
}

#[cfg(target_os = "macos")]
mod sys {
    extern "C" {
        fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
        fn munmap(addr: *mut u8, len: usize) -> i32;
        fn pthread_jit_write_protect_np(enabled: i32);
        fn sys_icache_invalidate(start: *mut u8, len: usize);
    }
    const PROT_RWX: i32 = 0x1 | 0x2 | 0x4;
    const MAP_PRIVATE_ANON_JIT: i32 = 0x0002 | 0x1000 | 0x0800;

    pub unsafe fn alloc(len: usize) -> *mut u8 {
        let mem = mmap(std::ptr::null_mut(), len, PROT_RWX, MAP_PRIVATE_ANON_JIT, -1, 0);
        if mem as isize == -1 {
            return std::ptr::null_mut();
        }
        pthread_jit_write_protect_np(0);
        mem
    }

    pub unsafe fn make_exec(mem: *mut u8, len: usize) -> bool {
        pthread_jit_write_protect_np(1);
        sys_icache_invalidate(mem, len);
        true
    }

    pub unsafe fn free_exec(mem: *mut u8, len: usize) {
        munmap(mem, len);
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod sys {
    extern "C" {
        fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
        fn mprotect(addr: *mut u8, len: usize, prot: i32) -> i32;
        fn munmap(addr: *mut u8, len: usize) -> i32;
    }
    const PROT_READ: i32 = 1;
    const PROT_WRITE: i32 = 2;
    const PROT_EXEC: i32 = 4;
    const MAP_PRIVATE_ANON: i32 = 0x02 | 0x20;

    #[cfg(target_arch = "aarch64")]
    unsafe fn flush_icache(start: *mut u8, len: usize) {
        use core::arch::asm;
        let ctr: usize;
        asm!("mrs {ctr}, ctr_el0", ctr = out(reg) ctr, options(nostack, preserves_flags));
        let dline = 4usize << ((ctr >> 16) & 0xf);
        let iline = 4usize << (ctr & 0xf);
        let end = start as usize + len;
        let mut p = (start as usize) & !(dline - 1);
        while p < end {
            asm!("dc cvau, {p}", p = in(reg) p, options(nostack, preserves_flags));
            p += dline;
        }
        asm!("dsb ish", options(nostack, preserves_flags));
        p = (start as usize) & !(iline - 1);
        while p < end {
            asm!("ic ivau, {p}", p = in(reg) p, options(nostack, preserves_flags));
            p += iline;
        }
        asm!("dsb ish", "isb", options(nostack, preserves_flags));
    }

    #[cfg(not(target_arch = "aarch64"))]
    unsafe fn flush_icache(_: *mut u8, _: usize) {}

    pub unsafe fn alloc(len: usize) -> *mut u8 {
        let mem = mmap(std::ptr::null_mut(), len, PROT_READ | PROT_WRITE, MAP_PRIVATE_ANON, -1, 0);
        if mem as isize == -1 {
            return std::ptr::null_mut();
        }
        mem
    }

    pub unsafe fn make_exec(mem: *mut u8, len: usize) -> bool {
        flush_icache(mem, len);
        mprotect(mem, len, PROT_READ | PROT_EXEC) == 0
    }

    pub unsafe fn free_exec(mem: *mut u8, len: usize) {
        munmap(mem, len);
    }
}

#[cfg(windows)]
mod sys {
    #[link(name = "kernel32")]
    extern "system" {
        fn VirtualAlloc(addr: *mut u8, len: usize, kind: u32, protect: u32) -> *mut u8;
        fn VirtualProtect(addr: *mut u8, len: usize, protect: u32, old: *mut u32) -> i32;
        fn VirtualFree(addr: *mut u8, len: usize, kind: u32) -> i32;
        fn FlushInstructionCache(process: *mut u8, addr: *const u8, len: usize) -> i32;
        fn GetCurrentProcess() -> *mut u8;
    }
    const MEM_COMMIT_RESERVE: u32 = 0x1000 | 0x2000;
    const MEM_RELEASE: u32 = 0x8000;
    const PAGE_READWRITE: u32 = 0x04;
    const PAGE_EXECUTE_READ: u32 = 0x20;

    pub unsafe fn alloc(len: usize) -> *mut u8 {
        VirtualAlloc(std::ptr::null_mut(), len, MEM_COMMIT_RESERVE, PAGE_READWRITE)
    }

    pub unsafe fn make_exec(mem: *mut u8, len: usize) -> bool {
        let mut old = 0;
        VirtualProtect(mem, len, PAGE_EXECUTE_READ, &mut old) != 0
            && FlushInstructionCache(GetCurrentProcess(), mem, len) != 0
    }

    pub unsafe fn free_exec(mem: *mut u8, _len: usize) {
        VirtualFree(mem, 0, MEM_RELEASE);
    }
}
