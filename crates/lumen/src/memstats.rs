//! Opt-in memory accounting: `LUMEN_MEM_STATS=1`.
//!
//! Nothing here runs unless the variable is set: the checks are a cached `OnceLock<bool>` read at
//! a handful of cold points (extension install, program end). What it reports:
//!
//! - **process**: private (commit) bytes and working set, current and peak, from the OS;
//! - **address space**: a walk of the process's committed regions (Windows), split into
//!   private heap/other, image (copy-on-write data), mapped, and the calling thread's stack
//!   (reserved vs committed);
//! - **allocator**: system heap ownership (`HeapSummary` on Windows, optional `mallinfo2`
//!   on GNU/Linux), and with the
//!   `mem-stats` cargo feature exact live/peak counters kept by [`crate::fastalloc`] (the only
//!   counters on a hot path, hence the feature);
//! - **engine**: a walk of the realm by category (see `Engine::mem_report`).
//!
//! The report goes to stderr so it never mixes with a script's stdout.

use std::sync::OnceLock;

/// Whether `LUMEN_MEM_STATS` is set (to anything but `0`).
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var_os("LUMEN_MEM_STATS").is_some_and(|v| v != "0");
        // LUMEN_MEM_TRACE=<bytes>: a backtrace for every allocation at least that big.
        #[cfg(all(feature = "mem-stats", not(target_arch = "wasm32")))]
        if let Some(n) = on
            .then(|| std::env::var("LUMEN_MEM_TRACE").ok())
            .flatten()
            .and_then(|v| v.parse::<usize>().ok())
        {
            // LUMEN_MEM_TRACE_CAT=<id> (an index into `CAT_NAMES`), LUMEN_MEM_TRACE_EVERY=<n>.
            let var = |k| std::env::var(k).ok().and_then(|v| v.parse::<usize>().ok());
            lumen_common::fastalloc::trace_filter(
                var("LUMEN_MEM_TRACE_CAT").and_then(|c| u8::try_from(c).ok()),
                var("LUMEN_MEM_TRACE_EVERY").unwrap_or(1),
            );
            lumen_common::fastalloc::trace_allocations_from(n);
        }
        // LUMEN_MEM_SITES=<category name substring | all>: sampled live allocation sites.
        #[cfg(all(feature = "mem-stats", not(target_arch = "wasm32")))]
        if let Some(want) = on.then(|| std::env::var("LUMEN_MEM_SITES").ok()).flatten() {
            let want = want.to_lowercase();
            let mask = CAT_NAMES
                .iter()
                .enumerate()
                .filter(|(_, name)| want == "all" || name.to_lowercase().contains(&want))
                .fold(0u32, |m, (k, _)| m | 1 << k);
            lumen_common::memsites::enable(mask);
        }
        on
    })
}

/// What an allocation is for, as far as the allocating code path knows: the current category
/// is a thread-local set by [`enter`] guards at the engine's phase boundaries (parsing, AST
/// decode, bytecode compile, …). With the `mem-stats` feature the allocator tags every block
/// with it, so the report can say how many live bytes each phase left behind. Without the
/// feature the guards compile to nothing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Cat {
    /// Nothing more specific: host/std allocations, the runtime's Rust state.
    Other = 0,
    /// Over-aligned allocations without category headers, including object-slab chunks.
    Slab = 1,
    /// Source text kept by the parser and the AST it builds (script, module, lazy bodies).
    Parse = 2,
    /// AST decoded from a snapshot / precompiled blob (function headers and deferred bodies).
    AstDecode = 3,
    /// Bytecode compiled at run time: chunks, constant pools, inline-cache vectors.
    Compile = 4,
    /// Precompiled bytecode chunks decoded from a blob.
    ChunkDecode = 5,
    /// Decompressed precompiled store blocks (bodies, bytecode, kept text).
    AotStore = 6,
    /// Engine construction: the built-in intrinsics (`Engine::new`).
    Builtins = 7,
    /// Running host extensions' JS glue (lumen-node, lumen-web, …) at install.
    Glue = 8,
    /// Running the user's program (objects' out-of-line storage, strings, scopes, …).
    Runtime = 9,
    /// Stack traces: line tables, position data, rendered stacks.
    StackTrace = 10,
    /// JIT: compile scratch (IR, register allocation) and the metadata kept with native code; the
    /// machine code itself is mapped outside the allocator and is not counted.
    Jit = 11,
    /// Native HTML DOM, style, layout, text, display-list and image-render allocations.
    Html = lumen_common::memcat::HTML_CATEGORY_ID,
    /// ArrayBuffer, typed-array, Blob and other JS byte-store backing bytes.
    Buffers = lumen_common::memcat::BUFFERS_CATEGORY_ID,
    /// HTTP/XHR/fetch response bodies and the HTTP cache's reads.
    Network = lumen_common::memcat::NETWORK_CATEGORY_ID,
    /// Decoded image pixels and the decode cache.
    Image = lumen_common::memcat::IMAGE_CATEGORY_ID,
    /// Decoded audio buffers.
    Audio = lumen_common::memcat::AUDIO_CATEGORY_ID,
    /// Canvas surfaces and 2D-context buffers.
    Canvas = lumen_common::memcat::CANVAS_CATEGORY_ID,
    /// JS string allocations (while no more specific phase is current).
    Strings = lumen_common::memcat::STRINGS_CATEGORY_ID,
    /// The embedding host's own structures.
    Host = lumen_common::memcat::HOST_CATEGORY_ID,
    /// Object property storage: named-entry vectors owned by objects.
    Props = lumen_common::memcat::PROPS_CATEGORY_ID,
    /// Array element storage.
    Arrays = lumen_common::memcat::ARRAYS_CATEGORY_ID,
    /// Scopes and environments: binding maps.
    Scopes = lumen_common::memcat::SCOPES_CATEGORY_ID,
    /// Map, Set, WeakMap and WeakSet storage.
    Collections = lumen_common::memcat::COLLECTIONS_CATEGORY_ID,
    /// Closure and function-object metadata.
    Functions = lumen_common::memcat::FUNCTIONS_CATEGORY_ID,
    /// Shapes and shape transitions.
    Shapes = lumen_common::memcat::SHAPES_CATEGORY_ID,
    /// Script source text handed over by the embedder.
    Source = lumen_common::memcat::SOURCE_CATEGORY_ID,
}

pub const CAT_NAMES: [&str; 27] = [
    "other (host/std)",
    "over-aligned blocks / object slab chunks",
    "parse: source + AST",
    "AST decoded from blobs",
    "bytecode compiled",
    "bytecode decoded (AOT)",
    "AOT store blocks",
    "builtins (Engine::new)",
    "extension glue init",
    "program run",
    "stack traces",
    "JIT compile + metadata",
    "HTML DOM and rendering",
    "ArrayBuffer/typed-array/Blob bytes",
    "network bodies + HTTP cache",
    "image decode + cache",
    "audio decode",
    "canvas surfaces",
    "JS strings",
    "host glue (embedder)",
    "object property storage",
    "array element storage",
    "scopes (binding maps)",
    "Map/Set/Weak* storage",
    "function/closure metadata",
    "shapes + transitions",
    "script source text",
];

/// The current thread's allocation category (see [`Cat`]).
#[cfg(feature = "mem-stats")]
#[inline(always)]
pub fn current_cat() -> u8 {
    lumen_common::memcat::current()
}

/// Restores the previous category on drop (see [`enter`]).
pub type CatGuard = lumen_common::memcat::Guard;

/// Attribute this thread's allocations to `cat` until the guard drops. Free without the
/// `mem-stats` feature.
#[inline(always)]
pub fn enter(cat: Cat) -> CatGuard {
    lumen_common::memcat::enter(lumen_common::memcat::CategoryTag::from_id(cat as u8))
}

/// Like [`enter`], but only while the current category is `Other`, `Runtime` or the embedder's `Host`; inside any
/// more specific phase (parsing, compiling, HTML, …) the allocation stays with that phase.
#[inline(always)]
pub fn enter_refining(cat: Cat) -> CatGuard {
    lumen_common::memcat::enter_if_generic(lumen_common::memcat::CategoryTag::from_id(cat as u8))
}

/// Live bytes by category and block-size bucket (feature `mem-stats`; zeros without it).
fn size_buckets() -> [[isize; 8]; 32] {
    #[cfg(all(feature = "mem-stats", not(target_arch = "wasm32")))]
    {
        lumen_common::fastalloc::size_buckets()
    }
    #[cfg(not(all(feature = "mem-stats", not(target_arch = "wasm32"))))]
    {
        [[0; 8]; 32]
    }
}

pub fn categories() -> Option<[isize; 32]> {
    #[cfg(all(feature = "mem-stats", not(target_arch = "wasm32")))]
    {
        Some(lumen_common::fastalloc::category_bytes())
    }
    #[cfg(not(all(feature = "mem-stats", not(target_arch = "wasm32"))))]
    {
        None
    }
}

/// Cumulative allocation requests and successful reallocation requests with alignment greater
/// than 16, grouped by the category active when each request was made. The byte values are
/// logical requested sizes, not live bytes; summing their per-profile delta with sampled
/// category live bytes is a conservative upper bound for allocations whose alignment prevents
/// header-based live attribution. Returns `None` when `mem-stats` is disabled.
pub fn overaligned_requests_by_category() -> Option<([usize; 32], [usize; 32])> {
    #[cfg(all(feature = "mem-stats", not(target_arch = "wasm32")))]
    {
        Some(lumen_common::fastalloc::overaligned_by_category())
    }
    #[cfg(not(all(feature = "mem-stats", not(target_arch = "wasm32"))))]
    {
        None
    }
}

/// Exact outstanding count and logical requested live bytes for over-aligned allocations by
/// original allocation category. The final flag is true if the bounded allocator-side ledger
/// overflowed or lost a pointer, in which case per-category attribution is incomplete. Returns
/// `None` when `mem-stats` is disabled.
pub fn overaligned_live_by_category() -> Option<([isize; 32], [isize; 32], bool)> {
    #[cfg(all(feature = "mem-stats", not(target_arch = "wasm32")))]
    {
        Some(lumen_common::fastalloc::overaligned_live_by_category())
    }
    #[cfg(not(all(feature = "mem-stats", not(target_arch = "wasm32"))))]
    {
        None
    }
}

/// Outstanding normal-alignment allocation counts and the live inner-layout bytes used by
/// `ClassAlloc` for each category. The byte values include the 16-byte category header and any
/// ClassAlloc size-class rounding. Over-aligned blocks are not represented because they store
/// no category header; use [`overaligned_live_by_category`] for their ledger-tracked live bytes.
pub fn category_allocation_overhead() -> Option<([isize; 32], [isize; 32])> {
    #[cfg(all(feature = "mem-stats", not(target_arch = "wasm32")))]
    {
        Some(lumen_common::fastalloc::category_allocation_overhead())
    }
    #[cfg(not(all(feature = "mem-stats", not(target_arch = "wasm32"))))]
    {
        None
    }
}

/// OS-level memory counters of this process, in bytes.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessMemory {
    /// Private committed bytes (Windows "private bytes" / commit charge; Linux `RssAnon`).
    pub private: usize,
    /// Peak of [`Self::private`] (Windows `PeakPagefileUsage`; Linux: 0 = unknown).
    pub peak_private: usize,
    /// Resident set / working set.
    pub working_set: usize,
    /// Peak working set (Linux `VmHWM`).
    pub peak_working_set: usize,
}

#[cfg(windows)]
mod win {
    use std::ffi::c_void;

    #[repr(C)]
    #[derive(Default)]
    pub struct ProcessMemoryCounters {
        pub cb: u32,
        pub page_fault_count: u32,
        pub peak_working_set_size: usize,
        pub working_set_size: usize,
        pub quota_peak_paged_pool_usage: usize,
        pub quota_paged_pool_usage: usize,
        pub quota_peak_non_paged_pool_usage: usize,
        pub quota_non_paged_pool_usage: usize,
        pub pagefile_usage: usize,
        pub peak_pagefile_usage: usize,
    }

    #[repr(C)]
    #[derive(Default)]
    pub struct HeapSummaryT {
        pub cb: u32,
        pub allocated: usize,
        pub committed: usize,
        pub reserved: usize,
        pub max_reserve: usize,
    }

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    pub struct MemoryBasicInformation {
        pub base_address: usize,
        pub allocation_base: usize,
        pub allocation_protect: u32,
        #[cfg(target_pointer_width = "64")]
        pub partition_id: u16,
        pub region_size: usize,
        pub state: u32,
        pub protect: u32,
        pub kind: u32,
    }

    pub const MEM_COMMIT: u32 = 0x1000;
    pub const MEM_PRIVATE: u32 = 0x20000;
    pub const MEM_MAPPED: u32 = 0x40000;
    pub const MEM_IMAGE: u32 = 0x1000000;
    pub const PAGE_READWRITE: u32 = 0x04;
    pub const PAGE_EXECUTE_READWRITE: u32 = 0x40;
    pub const PAGE_EXECUTE_READ: u32 = 0x20;
    pub const PAGE_GUARD: u32 = 0x100;

    #[link(name = "kernel32")]
    extern "system" {
        pub fn GetCurrentProcess() -> *mut c_void;
        pub fn K32GetProcessMemoryInfo(
            process: *mut c_void,
            counters: *mut ProcessMemoryCounters,
            cb: u32,
        ) -> i32;
        pub fn GetProcessHeap() -> *mut c_void;
        pub fn HeapSummary(heap: *mut c_void, flags: u32, summary: *mut HeapSummaryT) -> i32;
        pub fn GetProcessHeaps(count: u32, heaps: *mut *mut c_void) -> u32;
        pub fn VirtualQuery(
            addr: *const c_void,
            info: *mut MemoryBasicInformation,
            len: usize,
        ) -> usize;
        pub fn GetCurrentThreadStackLimits(low: *mut usize, high: *mut usize);
    }
}

/// This process's memory counters, when the platform exposes them.
pub fn process_memory() -> Option<ProcessMemory> {
    #[cfg(windows)]
    unsafe {
        let mut c = win::ProcessMemoryCounters {
            cb: std::mem::size_of::<win::ProcessMemoryCounters>() as u32,
            ..Default::default()
        };
        if win::K32GetProcessMemoryInfo(win::GetCurrentProcess(), &mut c, c.cb) == 0 {
            return None;
        }
        Some(ProcessMemory {
            private: c.pagefile_usage,
            peak_private: c.peak_pagefile_usage,
            working_set: c.working_set_size,
            peak_working_set: c.peak_working_set_size,
        })
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        let kb = |key: &str| -> usize {
            status
                .lines()
                .find_map(|l| l.strip_prefix(key))
                .and_then(|r| r.split_whitespace().next())
                .and_then(|n| n.parse::<usize>().ok())
                .unwrap_or(0)
                * 1024
        };
        Some(ProcessMemory {
            private: kb("RssAnon:"),
            peak_private: 0,
            working_set: kb("VmRSS:"),
            peak_working_set: kb("VmHWM:"),
        })
    }
    #[cfg(target_os = "macos")]
    {
        extern "C" {
            fn getpid() -> i32;
            fn proc_pid_rusage(pid: i32, flavor: i32, buffer: *mut u64) -> i32;
        }
        // rusage_info_v0: a 16-byte uuid, then u64 counters; resident size is word 8, physical footprint word 9.
        let mut buf = [0u64; 32];
        if unsafe { proc_pid_rusage(getpid(), 0, buf.as_mut_ptr()) } != 0 {
            return None;
        }
        Some(ProcessMemory {
            private: buf[9] as usize,
            peak_private: 0,
            working_set: buf[8] as usize,
            peak_working_set: 0,
        })
    }
    #[cfg(not(any(windows, unix)))]
    {
        None
    }
}

/// GNU allocator ownership counters. Arena totals include free blocks; mapped
/// totals cover malloc's separate mappings. Neither is a resident-memory count.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
mod gnu_heap {
    use std::ffi::{c_char, c_void};
    use std::sync::OnceLock;

    // glibc's public mallinfo2 ABI: ten size_t fields, in this order.
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub(super) struct Info {
        pub arena: usize,
        ordblks: usize,
        smblks: usize,
        hblks: usize,
        pub mapped: usize,
        usmblks: usize,
        fsmblks: usize,
        pub allocated: usize,
        pub free: usize,
        keepcost: usize,
    }

    type Query = unsafe extern "C" fn() -> Info;

    #[link(name = "dl")]
    unsafe extern "C" {
        fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    }

    pub(super) fn query() -> Option<Info> {
        static QUERY: OnceLock<Option<Query>> = OnceLock::new();
        let query = QUERY.get_or_init(|| {
            // RTLD_DEFAULT is null on GNU/Linux. mallinfo2 arrived in glibc 2.33;
            // optional lookup keeps older glibc builds runnable without counters.
            let address = unsafe { dlsym(std::ptr::null_mut(), c"mallinfo2".as_ptr()) };
            if address.is_null() {
                None
            } else {
                // POSIX dlsym permits conversion to the symbol's function type.
                // Info mirrors the public ABI and the symbol takes no arguments.
                Some(unsafe { std::mem::transmute::<*mut c_void, Query>(address) })
            }
        });
        query.map(|query| unsafe { query() })
    }
}

fn gnu_heap_report() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    if let Some(info) = gnu_heap::query() {
        eprintln!(
            "[mem] GNU heap: arena {} allocated {} free {} mapped {}",
            fmt_bytes(info.arena),
            fmt_bytes(info.allocated),
            fmt_bytes(info.free),
            fmt_bytes(info.mapped),
        );
    }
}

/// `(allocated, committed)` bytes of every heap in the process (Windows `HeapSummary`): what the
/// system allocator hands out to Rust, and what it keeps committed to do so.
pub fn system_heap() -> Option<(usize, usize)> {
    #[cfg(windows)]
    unsafe {
        let mut heaps = [std::ptr::null_mut(); 64];
        let n = win::GetProcessHeaps(heaps.len() as u32, heaps.as_mut_ptr()) as usize;
        let n = if n == 0 || n > heaps.len() {
            heaps[0] = win::GetProcessHeap();
            1
        } else {
            n
        };
        let (mut alloc, mut commit) = (0, 0);
        for &h in &heaps[..n] {
            let mut s = win::HeapSummaryT {
                cb: std::mem::size_of::<win::HeapSummaryT>() as u32,
                ..Default::default()
            };
            if win::HeapSummary(h, 0, &mut s) != 0 {
                alloc += s.allocated;
                commit += s.committed;
            }
        }
        Some((alloc, commit))
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// Committed bytes of the address space, by kind (Windows).
#[derive(Default, Debug, Clone, Copy)]
pub struct AddressSpace {
    /// Committed private read-write memory (heaps, slabs, stacks).
    pub private_rw: usize,
    /// Committed private executable memory (JIT code).
    pub private_exec: usize,
    /// Committed private memory with other protections (guard pages, read-only).
    pub private_other: usize,
    /// Written (copy-on-write) or writable pages of loaded images: an image's `.data`.
    pub image_rw: usize,
    /// Committed file-mapped memory.
    pub mapped: usize,
    /// The calling thread's stack: reserved and committed bytes.
    pub stack_reserved: usize,
    pub stack_committed: usize,
    /// The largest committed private regions `(bytes, protect)`.
    pub largest: [(usize, u32); 8],
}

/// Walk the address space (Windows only).
pub fn address_space() -> Option<AddressSpace> {
    #[cfg(windows)]
    unsafe {
        let mut a = AddressSpace::default();
        let (mut lo, mut hi) = (0usize, 0usize);
        win::GetCurrentThreadStackLimits(&mut lo, &mut hi);
        a.stack_reserved = hi - lo;
        let mut addr = 0usize;
        let mut info = win::MemoryBasicInformation::default();
        loop {
            let got = win::VirtualQuery(
                addr as *const _,
                &mut info,
                std::mem::size_of::<win::MemoryBasicInformation>(),
            );
            if got == 0 {
                break;
            }
            let size = info.region_size;
            if info.state == win::MEM_COMMIT {
                let in_stack = info.base_address >= lo && info.base_address < hi;
                if in_stack {
                    a.stack_committed += size;
                } else if info.kind == win::MEM_PRIVATE {
                    let p = info.protect & 0xff;
                    if p == win::PAGE_READWRITE && info.protect & win::PAGE_GUARD == 0 {
                        a.private_rw += size;
                    } else if p == win::PAGE_EXECUTE_READ || p == win::PAGE_EXECUTE_READWRITE {
                        a.private_exec += size;
                    } else {
                        a.private_other += size;
                    }
                    let min = a.largest.iter_mut().min_by_key(|e| e.0).expect("non-empty");
                    if size > min.0 {
                        *min = (size, info.protect);
                    }
                } else if info.kind == win::MEM_IMAGE {
                    // Copy-on-write pages that were written show as PAGE_READWRITE.
                    if info.protect & 0xff == win::PAGE_READWRITE {
                        a.image_rw += size;
                    }
                } else if info.kind == win::MEM_MAPPED {
                    a.mapped += size;
                }
            }
            match info.base_address.checked_add(size) {
                Some(next) if next > addr => addr = next,
                _ => break,
            }
        }
        a.largest.sort_by_key(|x| std::cmp::Reverse(x.0));
        Some(a)
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// Allocator counters kept by [`crate::fastalloc`] with the `mem-stats` feature:
/// `(live bytes requested, peak live bytes, bytes held in the free-list caches, allocations)`.
pub fn allocator() -> Option<(usize, usize, usize, usize)> {
    #[cfg(all(feature = "mem-stats", not(target_arch = "wasm32")))]
    {
        Some(lumen_common::fastalloc::counters())
    }
    #[cfg(not(all(feature = "mem-stats", not(target_arch = "wasm32"))))]
    {
        None
    }
}

/// Bytes → a short human string.
pub fn fmt_bytes(n: usize) -> String {
    if n >= 10 << 20 {
        format!("{:.1} MB", n as f64 / (1 << 20) as f64)
    } else if n >= 10 << 10 {
        format!("{:.0} KB", n as f64 / 1024.0)
    } else {
        format!("{n} B")
    }
}

/// One line of process-level numbers, labelled: for phase checkpoints.
pub fn phase(label: &str) {
    if !enabled() {
        return;
    }
    let mut line = format!("[mem] {label:<28}");
    if let Some(p) = process_memory() {
        line.push_str(&format!(
            " private {:>9} ws {:>9}",
            fmt_bytes(p.private),
            fmt_bytes(p.working_set)
        ));
    }
    if let Some((alloc, commit)) = system_heap() {
        line.push_str(&format!(
            " | sys heap alloc {:>9} commit {:>9}",
            fmt_bytes(alloc),
            fmt_bytes(commit)
        ));
    }
    if let Some((live, _, cached, _)) = allocator() {
        line.push_str(&format!(
            " | live {:>9} cached {:>9}",
            fmt_bytes(live),
            fmt_bytes(cached)
        ));
        if let Some(cats) = categories() {
            let slab = cats[Cat::Slab as usize].max(0) as usize;
            line.push_str(&format!(
                " excl. slab chunks {:>9}",
                fmt_bytes(live.saturating_sub(slab))
            ));
            if std::env::var_os("LUMEN_MEM_CATS").is_some() {
                line.push_str(&format!(" [shapes={}]", crate::value::shape_count()));
                for (k, name) in CAT_NAMES
                    .iter()
                    .enumerate()
                    .filter(|(k, _)| *k != Cat::Slab as usize)
                {
                    line.push_str(&format!(
                        " [{}={}]",
                        name.split(' ').next().unwrap_or(""),
                        cats[k] / 1024
                    ));
                }
            }
        }
    }
    eprintln!("{line}");
    gnu_heap_report();
}

/// The process-level part of the final report.
pub fn report_process() {
    if !enabled() {
        return;
    }
    eprintln!("[mem] ---- process ----");
    if let Some(p) = process_memory() {
        eprintln!(
            "[mem] private {} (peak {}), working set {} (peak {})",
            fmt_bytes(p.private),
            fmt_bytes(p.peak_private),
            fmt_bytes(p.working_set),
            fmt_bytes(p.peak_working_set)
        );
    }
    if let Some(a) = address_space() {
        eprintln!(
            "[mem] committed: private rw {}, exec {}, other {}; image rw {}; mapped {}",
            fmt_bytes(a.private_rw),
            fmt_bytes(a.private_exec),
            fmt_bytes(a.private_other),
            fmt_bytes(a.image_rw),
            fmt_bytes(a.mapped)
        );
        eprintln!(
            "[mem] this thread's stack: reserved {}, committed {}",
            fmt_bytes(a.stack_reserved),
            fmt_bytes(a.stack_committed)
        );
        let big: Vec<String> = a
            .largest
            .iter()
            .filter(|e| e.0 > 0)
            .map(|e| format!("{}@{:#x}", fmt_bytes(e.0), e.1))
            .collect();
        eprintln!("[mem] largest private regions: {}", big.join(", "));
    }
    gnu_heap_report();
    if let Some((alloc, commit)) = system_heap() {
        eprintln!(
            "[mem] system heaps: allocated {}, committed {} (overhead/fragmentation {})",
            fmt_bytes(alloc),
            fmt_bytes(commit),
            fmt_bytes(commit.saturating_sub(alloc))
        );
    }
    if let Some((live, peak, cached, n)) = allocator() {
        eprintln!(
            "[mem] allocator: live {} (peak {}), free-list cached {}, {} allocations",
            fmt_bytes(live),
            fmt_bytes(peak),
            fmt_bytes(cached),
            n
        );
    }
}

impl crate::interpreter::Interp {
    /// `LUMEN_MEM_STATS=1`: print the process-level report and this realm's breakdown by
    /// category to stderr. Walks the whole realm; only for diagnostics.
    pub fn mem_report(&mut self) {
        report_process();
        #[cfg(all(feature = "mem-stats", not(target_arch = "wasm32")))]
        lumen_common::memsites::report();
        if let Some(cats) = categories() {
            eprintln!(
                "[mem] ---- live allocator bytes by category (what was current when allocated) ----"
            );
            let total: isize = cats.iter().sum();
            for (k, name) in CAT_NAMES.iter().enumerate() {
                if cats[k] != 0 {
                    eprintln!(
                        "[mem]   {name:<26} {:>10}  {:>4.1}%",
                        fmt_bytes(cats[k].max(0) as usize),
                        cats[k] as f64 * 100.0 / total.max(1) as f64
                    );
                }
            }
            eprintln!(
                "[mem]   {:<26} {:>10}",
                "total",
                fmt_bytes(total.max(0) as usize)
            );
            eprintln!(
                "[mem]   by block size:            <=32    <=64   <=128   <=256    <=1K    <=4K   <=64K   larger"
            );
            let sizes = size_buckets();
            for (k, name) in CAT_NAMES.iter().enumerate() {
                if cats[k] >= 256 << 10 {
                    let row: String = sizes[k]
                        .iter()
                        .map(|&b| format!(" {:>7}", fmt_bytes(b.max(0) as usize)))
                        .collect();
                    eprintln!("[mem]   {name:<22}{row}");
                }
            }
        }
        let mut w = crate::value::memwalk::walk();
        eprintln!("[mem] ---- engine structures (estimated) ----");
        let names = crate::value::SLOT_CLASS_NAMES;
        let mut slab_total = 0;
        for (k, &(chunks, used, bumped, size)) in w.slab.iter().enumerate() {
            slab_total += chunks * 256 * 1024;
            eprintln!(
                "[mem]   slab {:<8} {:>3} chunks ({}), {} slots x {} B in use = {}, {} free slots retained",
                names[k],
                chunks,
                fmt_bytes(chunks * 256 * 1024),
                used,
                size,
                fmt_bytes(used * size),
                bumped - used
            );
        }
        eprintln!(
            "[mem]   objects {} (closures {}); slab {}",
            w.objects,
            w.user_fns,
            fmt_bytes(slab_total)
        );
        #[cfg(feature = "embed")]
        if let Some(census) = self.host_object_census() {
            eprintln!(
                "[mem]   host instances {} (table {}), by class (live/dead x payload):",
                census.entries,
                fmt_bytes(census.table_bytes)
            );
            for (name, live, dead, bytes) in census.classes.iter().take(12) {
                eprintln!(
                    "[mem]     {name:<28} {live:>8} / {dead:<8} x {bytes} B = {}",
                    fmt_bytes((live + dead) * bytes)
                );
            }
        }
        let p = crate::value::memwalk::property_size();
        eprintln!(
            "[mem]   property entries owned {} used / {} reserved = {} ({} slack)",
            w.prop_len,
            w.prop_cap,
            fmt_bytes(w.prop_cap * p),
            fmt_bytes((w.prop_cap - w.prop_len) * p)
        );
        eprintln!(
            "[mem]   entry slack: inline {}, heap vectors {}",
            fmt_bytes(w.inline_slack * p),
            fmt_bytes(w.heap_slack * p)
        );
        eprintln!(
            "[mem]     inline cap 1, len 0: swept husks {}, empty arrays {}, other {}",
            w.swept_husks, w.empty_arrays, w.other_empty_cap1
        );
        for cap in [1, 2, 4, 8] {
            let row: Vec<String> = (0..=cap).map(|n| w.inline_hist[cap][n].to_string()).collect();
            eprintln!("[mem]     inline cap {cap} by len 0..: {}", row.join(" "));
        }
        for cap in 1..=16 {
            if w.heap_hist[cap].iter().sum::<usize>() > 0 {
                let row: Vec<String> = (0..=cap).map(|n| w.heap_hist[cap][n].to_string()).collect();
                eprintln!("[mem]     heap cap {cap} by len 0..: {}", row.join(" "));
            }
        }
        if w.split_views > 0 {
            eprintln!(
                "[mem]   split views {}: offsets {}, distinct sources {}",
                w.split_views,
                fmt_bytes(w.split_view_offsets),
                fmt_bytes(w.split_view_src)
            );
        }
        eprintln!(
            "[mem]   scopes {} = {}; shapes {} = {}",
            w.scopes,
            fmt_bytes(w.scope_bytes),
            w.shapes,
            fmt_bytes(w.shape_bytes)
        );
        eprintln!(
            "[mem]   function nodes {} = {} (headers); {} with a materialised body ({} top-level stmts); lazy registry {}",
            w.fn_nodes,
            fmt_bytes(w.fn_node_bytes),
            w.bodies,
            w.body_stmts,
            w.lazy_registry
        );
        w.largest_bodies.sort_unstable_by(|a, b| b.0.cmp(&a.0));
        for (stmts, name) in w.largest_bodies.iter().take(8) {
            eprintln!("[mem]     materialised body {name:?}: {stmts} top-level stmts");
        }
        eprintln!(
            "[mem]     node sizes: Function {} B, Param {} B, Expr {} B, Stmt {} B, Pattern {} B, LazyBody {} B; params {} ({} in vec capacity)",
            std::mem::size_of::<crate::ast::Function>(),
            std::mem::size_of::<crate::ast::Param>(),
            std::mem::size_of::<crate::ast::Expr>(),
            std::mem::size_of::<crate::ast::Stmt>(),
            std::mem::size_of::<crate::ast::Pattern>(),
            std::mem::size_of::<crate::ast::LazyBody>(),
            w.params,
            w.params_cap
        );
        let c = &w.chunk;
        eprintln!(
            "[mem]   bytecode chunks {} = {}: ops {}, pools {}, inline caches {}, positions {}, headers {} (vec slack {})",
            w.chunks,
            fmt_bytes(c.total()),
            fmt_bytes(c.ops),
            fmt_bytes(c.pools),
            fmt_bytes(c.ics),
            fmt_bytes(c.positions),
            fmt_bytes(c.header),
            fmt_bytes(c.slack)
        );
        let ic: Vec<String> = c.ic_parts.iter().map(|&b| fmt_bytes(b)).collect();
        eprintln!(
            "[mem]     ics: props/objmaps/names/paths/pins/caps/cappins/switch = {}",
            ic.join(" / ")
        );
        eprintln!(
            "[mem]     property cache sites {}, ever filled {}, with a mid hop {}",
            c.ic_sites, c.ic_filled, c.ic_deep
        );
        eprintln!(
            "[mem]     chunks with property caches never filled: {} ({})",
            c.ic_idle_chunks,
            fmt_bytes(c.ic_idle_bytes)
        );
        eprintln!(
            "[mem]     cache pages touched / total: 8: {}/{}, 16: {}/{}, 32: {}/{}",
            c.ic_pages[0], c.ic_page_total[0], c.ic_pages[1], c.ic_page_total[1], c.ic_pages[2], c.ic_page_total[2]
        );
        eprintln!(
            "[mem]   precompiled chunks registered, not decoded: {}",
            crate::bytecode::serialize::lazy_registered()
        );
        let (nb, bb, nj, jb) = crate::precompiled::store::cached_bytes();
        eprintln!(
            "[mem]   AOT store cache: {nb} blocks {}, {nj} joined slices {}; recent body blocks {}",
            fmt_bytes(bb),
            fmt_bytes(jb),
            fmt_bytes(crate::precompiled::store::recent_bytes())
        );
        eprintln!(
            "[mem]   module records {}; stub cache {}",
            self.module_recs.len(),
            fmt_bytes(std::mem::size_of_val(&**self.stub_cache.borrow()))
        );
        let [retained, unevaluated, evaluating, completed, len, cap, source] =
            self.module_program_memory();
        eprintln!(
            "[mem]   module programs retained={retained} unevaluated={unevaluated} evaluating={evaluating} completed_retained={completed}; stmts={len}/{cap} (slots {}); distinct captured source {}",
            fmt_bytes(cap * std::mem::size_of::<crate::ast::Stmt>()),
            fmt_bytes(source)
        );
    }
}
