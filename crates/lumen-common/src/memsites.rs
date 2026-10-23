//! Sampled live-allocation-site profiler (`LUMEN_MEM_SITES`, `mem-stats` only).
//!
//! Allocations of the selected categories are sampled by bytes (one per `RATE` bytes on average,
//! so blocks of `RATE` or more are nearly always sampled). A sampled block's call stack is
//! hashed into a fixed site table and the site id is stored in the block's in-band header, so a
//! free or realloc finds its site without a side table. Live bytes per site are estimated by
//! weighting each sampled block with `size / P(sampled)`.

use std::cell::Cell;
use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, AtomicUsize, Ordering::*};

const RATE: f64 = 65536.0;
const SLOTS: usize = 8192;
const DEPTH: usize = 32;
const SHOWN_FRAMES: usize = 8;
const TOP: usize = 25;

/// Bit `k` set: sample allocations of category `k`. Zero disables the profiler.
pub static MASK: AtomicU32 = AtomicU32::new(0);

struct Site {
    key: AtomicU64,
    live: AtomicI64,
    samples: AtomicI64,
    depth: AtomicUsize,
    frames: [AtomicUsize; DEPTH],
}

#[allow(clippy::declare_interior_mutable_const)]
const EMPTY: Site = Site {
    key: AtomicU64::new(0),
    live: AtomicI64::new(0),
    samples: AtomicI64::new(0),
    depth: AtomicUsize::new(0),
    frames: [const { AtomicUsize::new(0) }; DEPTH],
};
static SITES: [Site; SLOTS] = [EMPTY; SLOTS];

thread_local! {
    static COUNTDOWN: Cell<i64> = const { Cell::new(0) };
    static RNG: Cell<u64> = const { Cell::new(0) };
    static STACK: Cell<(usize, usize)> = const { Cell::new((0, 0)) };
}

pub fn enable(mask: u32) {
    MASK.store(mask, Relaxed);
}

fn weight(size: usize) -> i64 {
    let s = size.max(1) as f64;
    (s / (1.0 - (-s / RATE).exp())) as i64
}

fn next_interval() -> i64 {
    let mut x = RNG.with(|r| {
        let mut x = r.get();
        if x == 0 {
            x = (r as *const _ as u64) ^ 0x9E37_79B9_7F4A_7C15;
        }
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        r.set(x);
        x
    });
    x = (x >> 11).max(1);
    let u = x as f64 / (1u64 << 53) as f64;
    (-u.ln() * RATE) as i64 + 1
}

/// Returns the site id (nonzero) when this allocation is sampled.
#[inline(never)]
pub fn maybe_sample(size: usize) -> u32 {
    let left = COUNTDOWN.try_with(|c| {
        let v = c.get() - size as i64;
        c.set(v);
        v
    });
    match left {
        Ok(v) if v <= 0 => {}
        _ => return 0,
    }
    let _ = COUNTDOWN.try_with(|c| c.set(next_interval()));
    let mut frames = [0usize; DEPTH];
    let n = capture(&mut frames);
    if n == 0 {
        return 0;
    }
    let id = site_for(&frames[..n]);
    if id != 0 {
        let site = &SITES[id as usize - 1];
        site.live.fetch_add(weight(size), Relaxed);
        site.samples.fetch_add(1, Relaxed);
    }
    id
}

pub fn freed(site: u32, size: usize) {
    let s = &SITES[site as usize - 1];
    s.live.fetch_sub(weight(size), Relaxed);
    s.samples.fetch_sub(1, Relaxed);
}

pub fn resized(site: u32, old: usize, new: usize) {
    SITES[site as usize - 1]
        .live
        .fetch_add(weight(new) - weight(old), Relaxed);
}

fn site_for(frames: &[usize]) -> u32 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &f in frames {
        h = (h ^ f as u64).wrapping_mul(0x0100_0000_01b3);
    }
    let key = h | 1;
    let mut i = (h >> 7) as usize % SLOTS;
    for _ in 0..SLOTS {
        let site = &SITES[i];
        let cur = site.key.load(Acquire);
        if cur == key {
            return i as u32 + 1;
        }
        if cur == 0 && site.key.compare_exchange(0, key, AcqRel, Acquire).is_ok() {
            for (slot, &f) in site.frames.iter().zip(frames) {
                slot.store(f, Relaxed);
            }
            site.depth.store(frames.len(), Release);
            return i as u32 + 1;
        }
        if site.key.load(Acquire) == key {
            return i as u32 + 1;
        }
        i = (i + 1) % SLOTS;
    }
    0
}

#[cfg(target_os = "macos")]
extern "C" {
    fn pthread_self() -> usize;
    fn pthread_get_stackaddr_np(thread: usize) -> *mut u8;
    fn pthread_get_stacksize_np(thread: usize) -> usize;
}

#[cfg(all(target_os = "macos", any(target_arch = "aarch64", target_arch = "x86_64")))]
#[inline(never)]
fn capture(out: &mut [usize; DEPTH]) -> usize {
    let (lo, hi) = STACK
        .try_with(|s| {
            if s.get().1 == 0 {
                unsafe {
                    let t = pthread_self();
                    let top = pthread_get_stackaddr_np(t) as usize;
                    s.set((top - pthread_get_stacksize_np(t), top));
                }
            }
            s.get()
        })
        .unwrap_or((0, 0));
    let mut fp: usize;
    unsafe {
        #[cfg(target_arch = "aarch64")]
        std::arch::asm!("mov {}, x29", out(reg) fp, options(nomem, nostack));
        #[cfg(target_arch = "x86_64")]
        std::arch::asm!("mov {}, rbp", out(reg) fp, options(nomem, nostack));
    }
    let mut n = 0;
    while n < DEPTH && fp & 7 == 0 && fp >= lo && fp + 16 <= hi {
        let (next, ret) = unsafe { (*(fp as *const usize), *((fp + 8) as *const usize)) };
        let ret = ret & 0x0000_ffff_ffff_ffff;
        if ret == 0 {
            break;
        }
        out[n] = ret;
        n += 1;
        if next <= fp {
            break;
        }
        fp = next;
    }
    n
}

#[cfg(not(all(target_os = "macos", any(target_arch = "aarch64", target_arch = "x86_64"))))]
fn capture(_out: &mut [usize; DEPTH]) -> usize {
    let _ = &STACK;
    0
}

#[repr(C)]
struct DlInfo {
    fname: *const std::ffi::c_char,
    fbase: *mut std::ffi::c_void,
    sname: *const std::ffi::c_char,
    saddr: *mut std::ffi::c_void,
}

extern "C" {
    fn dladdr(addr: *const std::ffi::c_void, info: *mut DlInfo) -> i32;
}

fn dladdr_name(addr: usize) -> Option<(String, usize)> {
    let mut info = DlInfo {
        fname: std::ptr::null(),
        fbase: std::ptr::null_mut(),
        sname: std::ptr::null(),
        saddr: std::ptr::null_mut(),
    };
    if unsafe { dladdr(addr as *const _, &mut info) } == 0 {
        return None;
    }
    let name = if info.sname.is_null() {
        format!("{addr:#x}")
    } else {
        unsafe { std::ffi::CStr::from_ptr(info.sname) }
            .to_string_lossy()
            .into_owned()
    };
    Some((name, info.fbase as usize))
}

fn resolve(addrs: &[usize]) -> Vec<String> {
    let mut names: Vec<String> = addrs
        .iter()
        .map(|&a| dladdr_name(a).map_or_else(|| format!("{a:#x}"), |(n, _)| n))
        .collect();
    let Some((_, base)) = dladdr_name(report as fn() as usize) else {
        return names;
    };
    let Ok(exe) = std::env::current_exe() else {
        return names;
    };
    let output = std::process::Command::new("atos")
        .arg("-o")
        .arg(exe)
        .arg("-l")
        .arg(format!("{base:#x}"))
        .args(addrs.iter().map(|a| format!("{:#x}", a - 1)))
        .output();
    if let Ok(out) = output {
        let text = String::from_utf8_lossy(&out.stdout);
        let lines: Vec<&str> = text.lines().collect();
        if lines.len() == addrs.len() {
            for (name, line) in names.iter_mut().zip(lines) {
                if !line.starts_with("0x") {
                    *name = line.to_string();
                }
            }
        }
    }
    names
}

fn is_allocator_frame(name: &str) -> bool {
    ["memsites", "fastalloc", "__rust_", "alloc::alloc::", "memstats::"]
        .iter()
        .any(|p| name.contains(p))
}

/// Print the top live sites by estimated bytes to stderr.
pub fn report() {
    let mask = MASK.swap(0, Relaxed);
    if mask == 0 {
        return;
    }
    let mut sites: Vec<(i64, i64, usize)> = SITES
        .iter()
        .enumerate()
        .filter_map(|(i, s)| {
            let live = s.live.load(Relaxed);
            (live > 0).then(|| (live, s.samples.load(Relaxed), i))
        })
        .collect();
    sites.sort_unstable_by(|a, b| b.0.cmp(&a.0));
    let total: i64 = sites.iter().map(|s| s.0).sum();
    eprintln!(
        "[mem] ---- sampled live allocation sites (categories mask {mask:#x}, ~{:.1} MiB estimated, {} sites) ----",
        total as f64 / 1048576.0,
        sites.len()
    );
    sites.truncate(TOP);
    let mut addrs: Vec<usize> = sites
        .iter()
        .flat_map(|&(_, _, i)| {
            let n = SITES[i].depth.load(Acquire);
            SITES[i].frames[..n].iter().map(|f| f.load(Relaxed))
        })
        .collect();
    addrs.sort_unstable();
    addrs.dedup();
    let names = resolve(&addrs);
    for (live, samples, i) in sites {
        eprintln!(
            "[mem] {:>9.2} MiB ({:.1}%)  {samples} sampled live blocks",
            live as f64 / 1048576.0,
            live as f64 * 100.0 / total.max(1) as f64
        );
        let n = SITES[i].depth.load(Acquire);
        let mut shown = 0;
        for frame in &SITES[i].frames[..n] {
            let addr = frame.load(Relaxed);
            let name = &names[addrs.binary_search(&addr).unwrap()];
            if is_allocator_frame(name) {
                continue;
            }
            eprintln!("[mem]     {name}");
            shown += 1;
            if shown == SHOWN_FRAMES {
                break;
            }
        }
    }
    MASK.store(mask, Relaxed);
}
