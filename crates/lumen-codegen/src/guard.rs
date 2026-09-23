//! Guard-page support for bounds-check-free memory access (the V8/wasmtime scheme).
//!
//! A [`Region`] reserves a large range of address space and commits a prefix of it; the rest
//! stays inaccessible. Generated code addresses `base + zext(i32 index) + u32 offset` with no
//! explicit check: anything past the committed prefix lands in the reserved tail and faults.
//! [`install`] adds a process-wide fault handler that, for a fault inside a registered region
//! raised by registered generated code, resumes at the trampoline exactly as a trap stub would
//! (see [`crate::x64::trampoline`]): `rsp = [ctx + entry_sp]; ret` with `eax = code + 1`.
//!
//! The base never moves, so native code may keep it in a register across calls and `grow` only
//! commits more pages. Supported on x86-64 Windows and Linux; elsewhere [`install`] returns
//! false and callers keep explicit bounds checks.

use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Bytes a 32-bit index plus a 32-bit static offset can reach past the base (plus slack for the
/// widest access), rounded up: the reservation that makes every such access land in the region.
pub const FULL_RESERVATION: usize = (8 << 30) + (64 << 10);

pub struct Region {
    base: *mut u8,
    reserved: usize,
    committed: Cell<usize>,
    slot: usize,
}

impl Region {
    /// Reserve `reserved` bytes (none of them accessible yet).
    pub fn reserve(reserved: usize) -> Option<Region> {
        let base = unsafe { sys::reserve(reserved) };
        if base.is_null() {
            return None;
        }
        let slot = register(&REGIONS, base as usize, base as usize + reserved);
        Some(Region {
            base,
            reserved,
            committed: Cell::new(0),
            slot,
        })
    }

    pub fn base(&self) -> *mut u8 {
        self.base
    }

    pub fn reserved(&self) -> usize {
        self.reserved
    }

    pub fn committed(&self) -> usize {
        self.committed.get()
    }

    /// Make the first `len` bytes accessible (zero-filled when new). Never shrinks.
    pub fn commit(&self, len: usize) -> bool {
        let cur = self.committed.get();
        if len <= cur {
            return true;
        }
        if len > self.reserved {
            return false;
        }
        let page = 4096;
        let from = cur & !(page - 1);
        let to = (len + page - 1) & !(page - 1);
        if !unsafe { sys::commit(self.base.add(from), to - from) } {
            return false;
        }
        self.committed.set(to.min(self.reserved));
        true
    }
}

impl Drop for Region {
    fn drop(&mut self) {
        unregister(&REGIONS, self.slot);
        unsafe { sys::release(self.base, self.reserved) };
    }
}

// ---- registries (lock-free so the fault handler can read them) --------------------------------

const SLOTS: usize = 4096;
struct Ranges([(AtomicUsize, AtomicUsize); SLOTS]);

#[allow(clippy::declare_interior_mutable_const)]
const EMPTY: (AtomicUsize, AtomicUsize) = (AtomicUsize::new(0), AtomicUsize::new(0));
static REGIONS: Ranges = Ranges([EMPTY; SLOTS]);
static CODE: Ranges = Ranges([EMPTY; SLOTS]);

fn register(r: &Ranges, start: usize, end: usize) -> usize {
    for (i, (s, e)) in r.0.iter().enumerate() {
        if s.compare_exchange(0, start, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
            e.store(end, Ordering::Release);
            return i;
        }
    }
    usize::MAX
}

fn unregister(r: &Ranges, slot: usize) {
    if let Some((s, e)) = r.0.get(slot) {
        e.store(0, Ordering::Release);
        s.store(0, Ordering::Release);
    }
}

fn contains(r: &Ranges, addr: usize) -> bool {
    r.0.iter().any(|(s, e)| {
        let s = s.load(Ordering::Acquire);
        s != 0 && addr >= s && addr < e.load(Ordering::Acquire)
    })
}

/// Marks `start..end` as generated code whose guard-page faults become traps. Dropping the
/// handle unregisters it.
pub struct CodeRange(usize);

impl CodeRange {
    pub fn new(start: usize, len: usize) -> CodeRange {
        CodeRange(register(&CODE, start, start + len))
    }
}

impl Drop for CodeRange {
    fn drop(&mut self) {
        unregister(&CODE, self.0);
    }
}

// ---- per-thread trap target ---------------------------------------------------------------------

thread_local! {
    /// `(ctx, entry_sp offset, trap code)` of the innermost active trampoline call.
    static TARGET: Cell<(usize, i32, u32)> = const { Cell::new((0, 0, 0)) };
}

/// Set the trap target for guard faults on this thread; returns the previous one, which the
/// caller restores when its trampoline call returns.
pub fn set_target(ctx: *mut u8, entry_sp_offset: i32, code: u32) -> (usize, i32, u32) {
    TARGET.with(|t| t.replace((ctx as usize, entry_sp_offset, code)))
}

pub fn restore_target(prev: (usize, i32, u32)) {
    TARGET.with(|t| t.set(prev));
}

/// For a fault at `pc` touching `addr`: the `(rsp, rip, rax)` to resume with, if it is ours.
fn redirect(pc: usize, addr: usize) -> Option<(u64, u64, u64)> {
    if !contains(&REGIONS, addr) || !contains(&CODE, pc) {
        return None;
    }
    let (ctx, off, code) = TARGET.with(|t| t.get());
    if ctx == 0 {
        return None;
    }
    // SAFETY: the trampoline stored its resume slot's address at [ctx + off].
    unsafe {
        let sp = *((ctx as isize + off as isize) as *const u64);
        let rip = *(sp as *const u64);
        Some((sp + 8, rip, code as u64 + 1))
    }
}

/// Install the fault handler (idempotent). False when unsupported on this platform.
pub fn install() -> bool {
    use std::sync::OnceLock;
    static OK: OnceLock<bool> = OnceLock::new();
    *OK.get_or_init(|| unsafe { sys::install() })
}

// ---- OS layers --------------------------------------------------------------------------------

#[cfg(windows)]
mod sys {
    #[link(name = "kernel32")]
    extern "system" {
        fn VirtualAlloc(addr: *mut u8, len: usize, kind: u32, protect: u32) -> *mut u8;
        fn VirtualFree(addr: *mut u8, len: usize, kind: u32) -> i32;
        fn AddVectoredExceptionHandler(first: u32, handler: usize) -> *mut u8;
    }
    const MEM_COMMIT: u32 = 0x1000;
    const MEM_RESERVE: u32 = 0x2000;
    const MEM_RELEASE: u32 = 0x8000;
    const PAGE_NOACCESS: u32 = 0x01;
    const PAGE_READWRITE: u32 = 0x04;

    pub unsafe fn reserve(len: usize) -> *mut u8 {
        VirtualAlloc(std::ptr::null_mut(), len, MEM_RESERVE, PAGE_NOACCESS)
    }
    pub unsafe fn commit(at: *mut u8, len: usize) -> bool {
        !VirtualAlloc(at, len, MEM_COMMIT, PAGE_READWRITE).is_null()
    }
    pub unsafe fn release(at: *mut u8, _len: usize) {
        VirtualFree(at, 0, MEM_RELEASE);
    }

    #[cfg(target_arch = "x86_64")]
    unsafe extern "system" fn handler(info: *mut [*mut u8; 2]) -> i32 {
        const CONTINUE_EXECUTION: i32 = -1;
        const CONTINUE_SEARCH: i32 = 0;
        let record = (*info)[0];
        let context = (*info)[1];
        // EXCEPTION_RECORD: code @0, NumberParameters @24, ExceptionInformation @32.
        if *(record as *const u32) != 0xC000_0005 || *(record.add(24) as *const u32) < 2 {
            return CONTINUE_SEARCH;
        }
        let addr = *(record.add(40) as *const usize);
        // CONTEXT (x64): Rax @0x78, Rsp @0x98, Rip @0xF8.
        let pc = *(context.add(0xF8) as *const usize);
        match super::redirect(pc, addr) {
            Some((rsp, rip, rax)) => {
                *(context.add(0x78) as *mut u64) = rax;
                *(context.add(0x98) as *mut u64) = rsp;
                *(context.add(0xF8) as *mut u64) = rip;
                CONTINUE_EXECUTION
            }
            None => CONTINUE_SEARCH,
        }
    }

    #[cfg(target_arch = "x86_64")]
    pub unsafe fn install() -> bool {
        !AddVectoredExceptionHandler(1, handler as *const () as usize).is_null()
    }
    #[cfg(not(target_arch = "x86_64"))]
    pub unsafe fn install() -> bool {
        false
    }
}

#[cfg(unix)]
mod sys {
    extern "C" {
        fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
        fn mprotect(addr: *mut u8, len: usize, prot: i32) -> i32;
        fn munmap(addr: *mut u8, len: usize) -> i32;
    }
    const PROT_NONE: i32 = 0;
    const PROT_RW: i32 = 1 | 2;
    const MAP_PRIVATE: i32 = 0x02;
    #[cfg(target_os = "macos")]
    const MAP_ANON: i32 = 0x1000;
    #[cfg(not(target_os = "macos"))]
    const MAP_ANON: i32 = 0x20;
    #[cfg(target_os = "macos")]
    const MAP_NORESERVE: i32 = 0x40;
    #[cfg(not(target_os = "macos"))]
    const MAP_NORESERVE: i32 = 0x4000;

    pub unsafe fn reserve(len: usize) -> *mut u8 {
        let p = mmap(
            std::ptr::null_mut(),
            len,
            PROT_NONE,
            MAP_PRIVATE | MAP_ANON | MAP_NORESERVE,
            -1,
            0,
        );
        if p as isize == -1 {
            std::ptr::null_mut()
        } else {
            p
        }
    }
    pub unsafe fn commit(at: *mut u8, len: usize) -> bool {
        mprotect(at, len, PROT_RW) == 0
    }
    pub unsafe fn release(at: *mut u8, len: usize) {
        munmap(at, len);
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    mod handler {
        /// glibc's `struct sigaction` on x86-64.
        #[repr(C)]
        pub struct SigAction {
            pub sa_sigaction: usize,
            pub sa_mask: [u64; 16],
            pub sa_flags: i32,
            pub sa_restorer: usize,
        }
        extern "C" {
            pub fn sigaction(sig: i32, act: *const SigAction, old: *mut SigAction) -> i32;
        }
        const SIGBUS: i32 = 7;
        const SIGSEGV: i32 = 11;
        const SA_SIGINFO: i32 = 4;
        const SA_ONSTACK: i32 = 0x0800_0000;
        const SA_NODEFER: i32 = 0x4000_0000;

        static mut PREV: [SigAction; 2] = unsafe { std::mem::zeroed() };

        unsafe extern "C" fn handler(sig: i32, info: *mut u8, uctx: *mut u8) {
            // siginfo_t.si_addr @16; ucontext_t.uc_mcontext.gregs @40: RAX=13, RSP=15, RIP=16.
            let addr = *(info.add(16) as *const usize);
            let gregs = uctx.add(40) as *mut u64;
            let pc = *gregs.add(16) as usize;
            if let Some((rsp, rip, rax)) = super::super::redirect(pc, addr) {
                *gregs.add(13) = rax;
                *gregs.add(15) = rsp;
                *gregs.add(16) = rip;
                return;
            }
            // Not ours: chain to the previous handler (e.g. Rust's stack-overflow reporter).
            let prev = &*std::ptr::addr_of!(PREV[(sig == SIGSEGV) as usize]);
            match prev.sa_sigaction {
                0 | 1 => {
                    // SIG_DFL / SIG_IGN: restore it and return, re-faulting into the default.
                    sigaction(sig, prev, std::ptr::null_mut());
                }
                h if prev.sa_flags & SA_SIGINFO != 0 => {
                    let f: unsafe extern "C" fn(i32, *mut u8, *mut u8) = std::mem::transmute(h);
                    f(sig, info, uctx)
                }
                h => {
                    let f: unsafe extern "C" fn(i32) = std::mem::transmute(h);
                    f(sig)
                }
            }
        }

        pub unsafe fn install() -> bool {
            let act = SigAction {
                sa_sigaction: handler as *const () as usize,
                sa_mask: [0; 16],
                sa_flags: SA_SIGINFO | SA_ONSTACK | SA_NODEFER,
                sa_restorer: 0,
            };
            let prev = &mut *std::ptr::addr_of_mut!(PREV);
            sigaction(SIGBUS, &act, &mut prev[0]) == 0 && sigaction(SIGSEGV, &act, &mut prev[1]) == 0
        }
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    pub unsafe fn install() -> bool {
        handler::install()
    }
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    pub unsafe fn install() -> bool {
        false
    }
}
