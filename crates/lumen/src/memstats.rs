//! Opt-in memory accounting: `LUMEN_MEM_STATS=1`.
//!
//! Nothing here runs unless the variable is set: the checks are a cached `OnceLock<bool>` read at
//! a handful of cold points (extension install, program end). What it reports:
//!
//! - **process**: private (commit) bytes and working set, current and peak, from the OS;
//! - **address space**: a walk of the process's committed regions (Windows), split into
//!   private heap/other, image (copy-on-write data), mapped, and the calling thread's stack
//!   (reserved vs committed);
//! - **allocator**: bytes the system heap holds (`HeapSummary` on Windows), and with the
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
            crate::fastalloc::trace_allocations_from(n);
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
    /// The object slab's chunks (over-aligned blocks; see `value::heap`).
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
    /// JIT: native code metadata.
    Jit = 11,
}

pub const CAT_NAMES: [&str; 12] = [
    "other (host/std)",
    "object slab chunks",
    "parse: source + AST",
    "AST decoded from blobs",
    "bytecode compiled",
    "bytecode decoded (AOT)",
    "AOT store blocks",
    "builtins (Engine::new)",
    "extension glue init",
    "program run",
    "stack traces",
    "JIT metadata",
];

#[cfg(feature = "mem-stats")]
thread_local! {
    static CUR_CAT: std::cell::Cell<u8> = const { std::cell::Cell::new(0) };
}

/// The current thread's allocation category (see [`Cat`]).
#[cfg(feature = "mem-stats")]
#[inline(always)]
pub fn current_cat() -> u8 {
    CUR_CAT.try_with(|c| c.get()).unwrap_or(0)
}

/// Restores the previous category on drop (see [`enter`]).
#[must_use]
pub struct CatGuard {
    #[cfg(feature = "mem-stats")]
    prev: u8,
}

#[cfg(feature = "mem-stats")]
impl Drop for CatGuard {
    fn drop(&mut self) {
        let _ = CUR_CAT.try_with(|c| c.set(self.prev));
    }
}

/// Attribute this thread's allocations to `cat` until the guard drops. Free without the
/// `mem-stats` feature.
#[inline(always)]
pub fn enter(cat: Cat) -> CatGuard {
    #[cfg(feature = "mem-stats")]
    {
        CatGuard {
            prev: CUR_CAT.try_with(|c| c.replace(cat as u8)).unwrap_or(0),
        }
    }
    #[cfg(not(feature = "mem-stats"))]
    {
        let _ = cat;
        CatGuard {}
    }
}

/// Live bytes by category and block-size bucket (feature `mem-stats`; zeros without it).
fn size_buckets() -> [[isize; 8]; 32] {
    #[cfg(all(feature = "mem-stats", not(target_arch = "wasm32")))]
    {
        crate::fastalloc::size_buckets()
    }
    #[cfg(not(all(feature = "mem-stats", not(target_arch = "wasm32"))))]
    {
        [[0; 8]; 32]
    }
}

pub fn categories() -> Option<[isize; 32]> {
    #[cfg(all(feature = "mem-stats", not(target_arch = "wasm32")))]
    {
        Some(crate::fastalloc::category_bytes())
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
    #[cfg(not(any(windows, all(unix, not(target_os = "macos")))))]
    {
        None
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
                    let min = a
                        .largest
                        .iter_mut()
                        .min_by_key(|e| e.0)
                        .expect("non-empty");
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
        Some(crate::fastalloc::counters())
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
    }
    eprintln!("{line}");
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
        if let Some(cats) = categories() {
            eprintln!("[mem] ---- live allocator bytes by category (what was current when allocated) ----");
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
            eprintln!("[mem]   {:<26} {:>10}", "total", fmt_bytes(total.max(0) as usize));
            eprintln!("[mem]   by block size:            <=32    <=64   <=128   <=256    <=1K    <=4K   <=64K   larger");
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
        let w = crate::value::memwalk::walk();
        eprintln!("[mem] ---- engine structures (estimated) ----");
        let names = ["plain", "inline4", "array", "inline8"];
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
        let p = crate::value::memwalk::property_size();
        eprintln!(
            "[mem]   property entries owned {} used / {} reserved = {} ({} slack)",
            w.prop_len,
            w.prop_cap,
            fmt_bytes(w.prop_cap * p),
            fmt_bytes((w.prop_cap - w.prop_len) * p)
        );
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
            fmt_bytes(self.stub_cache.len() * std::mem::size_of_val(&self.stub_cache[0]))
        );
    }
}
