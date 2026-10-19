//! Executable memory with a W^X policy: code is written into read-write pages, which are then
//! made read-execute (macOS uses `MAP_JIT` with the per-thread write-protect toggle).

use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

/// A bare-metal embedder's executable-page services. Allocation must return RW/NX
/// storage; sealing must finish cache maintenance and change it to RO/X.
#[derive(Clone, Copy)]
pub struct NativeBackend {
    pub alloc: unsafe extern "C" fn(usize) -> *mut u8,
    pub seal: unsafe extern "C" fn(*mut u8, usize) -> bool,
    pub free: unsafe extern "C" fn(*mut u8, usize),
}

static NATIVE_BACKEND: std::sync::OnceLock<NativeBackend> = std::sync::OnceLock::new();

/// Install before creating an engine. Callback correctness, exclusive mapping
/// ownership, and lifetime are the embedder's responsibility.
pub unsafe fn install_native_backend(backend: NativeBackend) -> bool {
    NATIVE_BACKEND.set(backend).is_ok()
}

pub fn native_backend_available() -> bool {
    NATIVE_BACKEND.get().is_some()
}

/// The error of an executable-memory allocation that found no room.
pub const EXEC_EXHAUSTED: &str = "jit: cannot allocate executable memory";

static EXEC_FREED: AtomicU64 = AtomicU64::new(0);
static EXEC_TRIM: AtomicU64 = AtomicU64::new(0);

/// Bumped whenever any [`ExecMemory`] is freed: a failed compile is worth retrying once it moves.
pub fn exec_freed_generation() -> u64 {
    EXEC_FREED.load(Ordering::Relaxed)
}

/// Bumped by [`request_exec_trim`] and by every failed executable allocation. Each engine
/// compares it at a safe point and releases its own cold code when it moved.
pub fn exec_trim_generation() -> u64 {
    EXEC_TRIM.load(Ordering::Relaxed)
}

/// Ask every engine, on whatever thread, to release cold JIT code at its next safe point (code
/// can only be freed by the thread that owns it). Cheap and callable from any thread.
pub fn request_exec_trim() {
    EXEC_TRIM.fetch_add(1, Ordering::Relaxed);
}

/// Size of this thread's most recent allocation that found no room.
pub fn last_failed_len() -> usize {
    LAST_FAILED.with(Cell::get)
}

/// A byte budget on the executable memory this thread allocates from now on (`None` lifts
/// it): allocations past it fail like an exhausted arena. Memory allocated under a budget
/// keeps counting against it until freed, wherever it is freed.
pub fn set_thread_exec_limit(bytes: Option<usize>) {
    THREAD_BUDGET.with(|b| {
        *b.borrow_mut() = bytes.map(|limit| {
            Arc::new(Budget {
                limit,
                used: AtomicUsize::new(0),
            })
        });
    });
}

struct Budget {
    limit: usize,
    used: AtomicUsize,
}

std::thread_local! {
    static THREAD_BUDGET: RefCell<Option<Arc<Budget>>> = const { RefCell::new(None) };
    static LAST_FAILED: Cell<usize> = const { Cell::new(0) };
}

/// A failed allocation asks the other engines to trim, unless only a thread budget (a stand-in
/// for an arena no one else shares) ran out.
fn exhausted(len: usize, shared: bool) -> String {
    LAST_FAILED.with(|l| l.set(len));
    if shared {
        request_exec_trim();
    }
    EXEC_EXHAUSTED.into()
}

/// A block of executable code, freed on drop.
pub struct ExecMemory {
    ptr: *mut u8,
    len: usize,
    budget: Option<Arc<Budget>>,
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
        let budget = THREAD_BUDGET.with(|b| b.borrow().clone());
        if let Some(b) = &budget {
            if b.used.fetch_add(len, Ordering::Relaxed) + len > b.limit {
                b.used.fetch_sub(len, Ordering::Relaxed);
                return Err(exhausted(len, false));
            }
        }
        let release = |budget: &Option<Arc<Budget>>| {
            if let Some(b) = budget {
                b.used.fetch_sub(len, Ordering::Relaxed);
            }
        };
        let ptr = unsafe { sys::alloc(len) };
        if ptr.is_null() {
            release(&budget);
            return Err(exhausted(len, true));
        }
        fill(
            unsafe { std::slice::from_raw_parts_mut(ptr, len) },
            ptr as u64,
        );
        if !unsafe { sys::make_exec(ptr, len) } {
            unsafe { sys::free_exec(ptr, len) };
            release(&budget);
            return Err("jit: cannot make memory executable".into());
        }
        Ok(ExecMemory { ptr, len, budget })
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

/// `len` bytes of zeroed, never-freed read-write memory straight from the OS (null where there
/// is none). Pages cost physical memory only once touched, unlike a zeroed heap block.
pub fn alloc_pages(len: usize) -> *mut u8 {
    // SAFETY: a fresh anonymous mapping; nothing else refers to it.
    unsafe { sys::alloc_data(len) }
}

/// Actual OS mapping granule used to separate executable code from writable GOT pages.
pub fn host_page_size() -> u32 {
    #[cfg(windows)]
    {
        #[repr(C)]
        struct SystemInfo {
            oem_id: u32,
            page_size: u32,
            minimum_address: *mut u8,
            maximum_address: *mut u8,
            processor_mask: usize,
            processor_count: u32,
            processor_type: u32,
            allocation_granularity: u32,
            processor_level: u16,
            processor_revision: u16,
        }
        #[link(name = "kernel32")]
        extern "system" {
            fn GetSystemInfo(info: *mut SystemInfo);
        }
        let mut info = core::mem::MaybeUninit::<SystemInfo>::uninit();
        unsafe {
            GetSystemInfo(info.as_mut_ptr());
            return info.assume_init().page_size;
        }
    }
    #[cfg(unix)]
    {
        unsafe extern "C" {
            fn getpagesize() -> i32;
        }
        return unsafe { getpagesize() }.max(0) as u32;
    }
    #[cfg(target_arch = "wasm32")]
    {
        return 65536;
    }
    #[cfg(not(any(windows, unix, target_arch = "wasm32")))]
    {
        4096
    }
}

/// Native image mappings share the OS page primitives with JIT mappings, but
/// seal only their code prefix; the following GOT pages remain writable.
#[cfg(any(windows, all(unix, not(target_os = "macos"))))]
pub(crate) unsafe fn alloc_native_image(len: usize) -> *mut u8 {
    sys::alloc_data(len)
}

#[cfg(any(windows, all(unix, not(target_os = "macos"))))]
pub(crate) unsafe fn seal_native_code(base: *mut u8, code_len: usize) -> bool {
    sys::make_exec(base, code_len)
}

#[cfg(any(windows, all(unix, not(target_os = "macos"))))]
pub(crate) unsafe fn free_native_image(base: *mut u8, len: usize) {
    sys::free_exec(base, len)
}

#[cfg(target_os = "macos")]
pub(crate) unsafe fn alloc_native_image(len: usize) -> *mut u8 {
    sys::alloc(len)
}

#[cfg(target_os = "macos")]
pub(crate) unsafe fn seal_native_image(base: *mut u8, mapped_len: usize, code_len: usize) -> bool {
    sys::seal_native_image(base, mapped_len, code_len)
}

#[cfg(target_os = "macos")]
pub(crate) unsafe fn free_native_image(base: *mut u8, len: usize) {
    sys::free_native_image(base, len)
}

/// An owned zeroed read-write OS mapping, released on drop. Unlike executable code,
/// it never changes permissions and can hold an engine's bounded scratch storage.
pub struct DataMemory {
    ptr: *mut u8,
    len: usize,
}

impl DataMemory {
    pub fn new(len: usize) -> Option<Self> {
        let len = len.max(1);
        let ptr = alloc_pages(len);
        if ptr.is_null() {
            None
        } else {
            Some(Self { ptr, len })
        }
    }

    pub fn as_ptr(&self) -> *mut u8 {
        self.ptr
    }
}

impl Drop for DataMemory {
    fn drop(&mut self) {
        // Both OS mapping families release data/code with the same unmap operation.
        unsafe { sys::free_exec(self.ptr, self.len) }
    }
}

impl Drop for ExecMemory {
    fn drop(&mut self) {
        unsafe { sys::free_exec(self.ptr, self.len) }
        if let Some(b) = &self.budget {
            b.used.fetch_sub(self.len, Ordering::Relaxed);
        }
        EXEC_FREED.fetch_add(1, Ordering::Relaxed);
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
        let mem = mmap(
            std::ptr::null_mut(),
            len,
            PROT_RWX,
            MAP_PRIVATE_ANON_JIT,
            -1,
            0,
        );
        if mem as isize == -1 {
            return std::ptr::null_mut();
        }
        pthread_jit_write_protect_np(0);
        mem
    }

    pub unsafe fn alloc_data(len: usize) -> *mut u8 {
        const PROT_RW: i32 = 0x1 | 0x2;
        const MAP_PRIVATE_ANON: i32 = 0x0002 | 0x1000;
        let mem = mmap(std::ptr::null_mut(), len, PROT_RW, MAP_PRIVATE_ANON, -1, 0);
        if mem as isize == -1 {
            return std::ptr::null_mut();
        }
        mem
    }

    pub unsafe fn make_exec(mem: *mut u8, len: usize) -> bool {
        pthread_jit_write_protect_np(1);
        sys_icache_invalidate(mem, len);
        true
    }

    pub unsafe fn seal_native_image(base: *mut u8, mapped_len: usize, code_len: usize) -> bool {
        // GOT pages need ordinary RW memory: the thread's JIT write-protect
        // state would otherwise make the mutable GOT inaccessible while running.
        let tail = base.add(code_len);
        let tail_len = mapped_len - code_len;
        let data = std::slice::from_raw_parts(tail, tail_len).to_vec();
        let remapped = mmap(tail, tail_len, 3, 0x0002 | 0x1000 | 0x0010, -1, 0);
        if remapped != tail {
            pthread_jit_write_protect_np(1);
            return false;
        }
        std::ptr::copy_nonoverlapping(data.as_ptr(), tail, tail_len);
        sys_icache_invalidate(base, code_len);
        // XNU refuses mprotect on MAP_JIT pages (EACCES); W^X for them is the
        // per-thread write-protect switch, which every new thread starts with on.
        pthread_jit_write_protect_np(1);
        true
    }

    pub unsafe fn free_native_image(mem: *mut u8, len: usize) {
        // Loading can fail before sealing. Restore the executing-thread policy
        // on that path as well as after a successfully sealed allocation.
        pthread_jit_write_protect_np(1);
        free_exec(mem, len);
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
        let mem = mmap(
            std::ptr::null_mut(),
            len,
            PROT_READ | PROT_WRITE,
            MAP_PRIVATE_ANON,
            -1,
            0,
        );
        if mem as isize == -1 {
            return std::ptr::null_mut();
        }
        mem
    }

    pub unsafe fn alloc_data(len: usize) -> *mut u8 {
        alloc(len)
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
        VirtualAlloc(
            std::ptr::null_mut(),
            len,
            MEM_COMMIT_RESERVE,
            PAGE_READWRITE,
        )
    }

    pub unsafe fn alloc_data(len: usize) -> *mut u8 {
        alloc(len)
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

/// No executable memory on targets without an OS memory API (wasm32): allocation fails and
/// callers fall back (on wasm32, to the WebAssembly backend).
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
mod sys {
    pub unsafe fn alloc(len: usize) -> *mut u8 {
        match super::NATIVE_BACKEND.get() {
            Some(backend) => unsafe { (backend.alloc)(len) },
            None => std::ptr::null_mut(),
        }
    }

    pub unsafe fn alloc_data(_: usize) -> *mut u8 {
        // Scratch data must not consume the bounded executable-page arena.
        std::ptr::null_mut()
    }

    pub unsafe fn make_exec(memory: *mut u8, len: usize) -> bool {
        super::NATIVE_BACKEND
            .get()
            .is_some_and(|backend| unsafe { (backend.seal)(memory, len) })
    }

    pub unsafe fn free_exec(memory: *mut u8, len: usize) {
        if let Some(backend) = super::NATIVE_BACKEND.get() {
            unsafe { (backend.free)(memory, len) }
        }
    }
}

#[cfg(not(any(unix, windows, all(target_arch = "aarch64", target_os = "none"))))]
mod sys {
    pub unsafe fn alloc(_len: usize) -> *mut u8 {
        std::ptr::null_mut()
    }
    pub unsafe fn alloc_data(_len: usize) -> *mut u8 {
        std::ptr::null_mut()
    }
    pub unsafe fn make_exec(_mem: *mut u8, _len: usize) -> bool {
        false
    }
    pub unsafe fn free_exec(_mem: *mut u8, _len: usize) {}
}

#[cfg(all(test, any(windows, unix)))]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    static GLOBAL_COUNTERS: Mutex<()> = Mutex::new(());

    fn serialize_counters() -> MutexGuard<'static, ()> {
        GLOBAL_COUNTERS.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn thread_limit_fails_allocation_past_the_budget_and_counts_frees() {
        let _serial = serialize_counters();
        set_thread_exec_limit(Some(300));
        let trim = exec_trim_generation();
        let a = ExecMemory::new(&[0xc3; 200]).unwrap();
        let freed = exec_freed_generation();
        assert_eq!(
            ExecMemory::new(&[0xc3; 200]).err().as_deref(),
            Some(EXEC_EXHAUSTED)
        );
        assert_eq!(last_failed_len(), 200);
        assert_eq!(
            exec_trim_generation(),
            trim,
            "a thread budget is not arena pressure"
        );
        drop(a);
        assert!(exec_freed_generation() > freed);
        let b = ExecMemory::new(&[0xc3; 200]).unwrap();
        set_thread_exec_limit(None);
        drop(b);
        assert!(ExecMemory::new(&[0xc3; 4096]).is_ok());
    }

    #[test]
    fn requesting_a_trim_moves_the_generation() {
        let _serial = serialize_counters();
        let before = exec_trim_generation();
        request_exec_trim();
        assert!(exec_trim_generation() > before);
    }
}
