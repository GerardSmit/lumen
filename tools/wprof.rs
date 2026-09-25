//! wprof: a tiny sampling profiler for Windows x64 that needs no administrator rights.
//!
//! It starts the program, suspends its threads about once per millisecond, reads each thread's
//! instruction pointer (and a short frame-pointer-free stack scan for callers), resumes them,
//! and afterwards resolves the addresses through dbghelp and the PDB next to the executable.
//!
//! Build:   rustc -O tools/wprof.rs -o target/wprof.exe
//! Run:     target/wprof.exe [-n TOP] [--callers] [--idle] -- target/fast/lumen-cli.exe bench.js
//!
//! For line numbers, build the profiled binary with line tables, e.g.
//! `CARGO_PROFILE_FAST_DEBUG=line-tables-only cargo build --profile fast -p lumen-cli`.
#![allow(non_snake_case, clippy::upper_case_acronyms)]

use std::collections::HashMap;
use std::ffi::c_void;
use std::os::windows::io::AsRawHandle;
use std::time::{Duration, Instant};

type HANDLE = *mut c_void;

#[repr(C)]
struct THREADENTRY32 {
    dwSize: u32,
    cntUsage: u32,
    th32ThreadID: u32,
    th32OwnerProcessID: u32,
    tpBasePri: i32,
    tpDeltaPri: i32,
    dwFlags: u32,
}

#[repr(C)]
struct MODULEENTRY32W {
    dwSize: u32,
    th32ModuleID: u32,
    th32ProcessID: u32,
    GlblcntUsage: u32,
    ProccntUsage: u32,
    modBaseAddr: *mut u8,
    modBaseSize: u32,
    hModule: HANDLE,
    szModule: [u16; 256],
    szExePath: [u16; 260],
}

#[repr(C, align(16))]
struct CONTEXT([u8; 1232]);
const CTX_FLAGS: usize = 0x30;
const CTX_RSP: usize = 0x98;
const CTX_RIP: usize = 0xF8;
const CONTEXT_CONTROL: u32 = 0x0010_0001;

#[repr(C)]
struct SYMBOL_INFO {
    SizeOfStruct: u32,
    TypeIndex: u32,
    Reserved: [u64; 2],
    Index: u32,
    Size: u32,
    ModBase: u64,
    Flags: u32,
    Value: u64,
    Address: u64,
    Register: u32,
    Scope: u32,
    Tag: u32,
    NameLen: u32,
    MaxNameLen: u32,
    Name: [u8; 1024],
}

#[repr(C)]
struct IMAGEHLP_LINE64 {
    SizeOfStruct: u32,
    Key: *mut c_void,
    LineNumber: u32,
    FileName: *const i8,
    Address: u64,
}

#[link(name = "kernel32")]
extern "system" {
    fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> HANDLE;
    fn Thread32First(snap: HANDLE, te: *mut THREADENTRY32) -> i32;
    fn Thread32Next(snap: HANDLE, te: *mut THREADENTRY32) -> i32;
    fn Module32FirstW(snap: HANDLE, me: *mut MODULEENTRY32W) -> i32;
    fn Module32NextW(snap: HANDLE, me: *mut MODULEENTRY32W) -> i32;
    fn OpenThread(access: u32, inherit: i32, tid: u32) -> HANDLE;
    fn SuspendThread(h: HANDLE) -> u32;
    fn ResumeThread(h: HANDLE) -> u32;
    fn GetThreadContext(h: HANDLE, ctx: *mut CONTEXT) -> i32;
    fn ReadProcessMemory(p: HANDLE, addr: *const c_void, buf: *mut c_void, n: usize, read: *mut usize) -> i32;
    fn CloseHandle(h: HANDLE) -> i32;
}

#[link(name = "dbghelp")]
extern "system" {
    fn SymSetOptions(opts: u32) -> u32;
    fn SymInitialize(p: HANDLE, path: *const i8, invade: i32) -> i32;
    fn SymLoadModuleExW(p: HANDLE, file: HANDLE, image: *const u16, module: *const u16, base: u64, size: u32, data: *mut c_void, flags: u32) -> u64;
    fn SymFromAddr(p: HANDLE, addr: u64, disp: *mut u64, sym: *mut SYMBOL_INFO) -> i32;
    fn SymGetLineFromAddr64(p: HANDLE, addr: u64, disp: *mut u32, line: *mut IMAGEHLP_LINE64) -> i32;
}

const INVALID: HANDLE = -1isize as HANDLE;

/// Readable form of a Rust v0 symbol (`_R...`): its identifiers joined by `::` (crate hashes,
/// generics punctuation and disambiguators dropped). Other names pass through.
fn demangle(s: &str) -> String {
    let body = s.strip_prefix("_R").or_else(|| s.strip_prefix('R')).filter(|b| b.starts_with(['N', 'I', 'C', 'X', 'Y', 'M']));
    let Some(body) = body else { return s.to_string() };
    let body = body.split(".llvm.").next().unwrap_or(body);
    let b = body.as_bytes();
    let mut out: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < b.len() {
        // A crate root `Cs<base62>_` is followed by the crate's name; skip the hash.
        if b[i] == b'C' && i + 1 < b.len() && b[i + 1] == b's' {
            i += 2;
            while i < b.len() && b[i] != b'_' {
                i += 1;
            }
            i += 1;
            continue;
        }
        if b[i].is_ascii_digit() && (i == 0 || !b[i - 1].is_ascii_digit()) {
            let mut j = i;
            let mut n = 0usize;
            while j < b.len() && b[j].is_ascii_digit() {
                n = n * 10 + (b[j] - b'0') as usize;
                j += 1;
            }
            if j < b.len() && b[j] == b'_' {
                j += 1;
            }
            if n > 0 && j + n <= b.len() {
                let id = &body[j..j + n];
                if id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                    out.push(id);
                    i = j + n;
                    continue;
                }
            }
        }
        i += 1;
    }
    if out.is_empty() {
        s.to_string()
    } else {
        out.join("::")
    }
}

/// Idle frames (a thread parked in the kernel), left out of the report by default.
fn is_wait(name: &str) -> bool {
    name.starts_with("ntdll.dll!") && (name.contains("Wait") || name.contains("Delay") || name.contains("RemoveIoCompletion"))
}

struct Module {
    base: u64,
    size: u64,
    name: String,
    path: Vec<u16>,
}

fn modules(pid: u32) -> Vec<Module> {
    let mut out = Vec::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(0x8 | 0x10, pid); // SNAPMODULE | SNAPMODULE32
        if snap == INVALID {
            return out;
        }
        let mut me: MODULEENTRY32W = std::mem::zeroed();
        me.dwSize = std::mem::size_of::<MODULEENTRY32W>() as u32;
        let mut ok = Module32FirstW(snap, &mut me);
        while ok != 0 {
            let z = |s: &[u16]| s[..s.iter().position(|&c| c == 0).unwrap_or(s.len())].to_vec();
            out.push(Module {
                base: me.modBaseAddr as u64,
                size: me.modBaseSize as u64,
                name: String::from_utf16_lossy(&z(&me.szModule)),
                path: z(&me.szExePath).into_iter().chain([0]).collect(),
            });
            ok = Module32NextW(snap, &mut me);
        }
        CloseHandle(snap);
    }
    out
}

fn threads(pid: u32) -> Vec<u32> {
    let mut out = Vec::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(0x4, 0); // SNAPTHREAD
        if snap == INVALID {
            return out;
        }
        let mut te: THREADENTRY32 = std::mem::zeroed();
        te.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
        let mut ok = Thread32First(snap, &mut te);
        while ok != 0 {
            if te.th32OwnerProcessID == pid {
                out.push(te.th32ThreadID);
            }
            ok = Thread32Next(snap, &mut te);
        }
        CloseHandle(snap);
    }
    out
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let sep = args.iter().position(|a| a == "--").expect("usage: wprof [-n TOP] [--callers] -- program args...");
    let mut top = 40usize;
    let callers = args[1..sep].iter().any(|a| a == "--callers");
    if let Some(i) = args[1..sep].iter().position(|a| a == "-n") {
        top = args[i + 2].parse().expect("-n N");
    }
    let mut child = std::process::Command::new(&args[sep + 1])
        .args(&args[sep + 2..])
        .spawn()
        .expect("spawn");
    let pid = child.id();
    let proc_h = child.as_raw_handle() as HANDLE;
    let mut samples: HashMap<u64, u64> = HashMap::new();
    let mut edges: HashMap<(u64, u64), u64> = HashMap::new();
    let mut open: HashMap<u32, HANDLE> = HashMap::new();
    let mut mods: Vec<Module> = Vec::new();
    let start = Instant::now();
    let mut last_scan = Instant::now() - Duration::from_secs(1);
    let mut total = 0u64;
    let mut stack = vec![0u64; 512];
    loop {
        if let Ok(Some(_)) = child.try_wait() {
            break;
        }
        if last_scan.elapsed() > Duration::from_millis(50) {
            for tid in threads(pid) {
                open.entry(tid).or_insert_with(|| unsafe { OpenThread(0x2 | 0x8 | 0x40, 0, tid) });
            }
            let m = modules(pid);
            if m.len() >= mods.len() {
                mods = m;
            }
            last_scan = Instant::now();
        }
        for &h in open.values() {
            if h.is_null() {
                continue;
            }
            unsafe {
                if SuspendThread(h) == u32::MAX {
                    continue;
                }
                let mut ctx: CONTEXT = std::mem::zeroed();
                (ctx.0.as_mut_ptr().add(CTX_FLAGS) as *mut u32).write(CONTEXT_CONTROL);
                if GetThreadContext(h, &mut ctx) != 0 {
                    let rip = (ctx.0.as_ptr().add(CTX_RIP) as *const u64).read();
                    *samples.entry(rip).or_default() += 1;
                    total += 1;
                    if callers {
                        // Heuristic caller: the first stack word that points into an executable
                        // module just after a call (good enough for leaf attribution).
                        let rsp = (ctx.0.as_ptr().add(CTX_RSP) as *const u64).read();
                        let mut got = 0usize;
                        ReadProcessMemory(proc_h, rsp as *const c_void, stack.as_mut_ptr() as *mut c_void, stack.len() * 8, &mut got);
                        if let Some(&ret) = stack[..got / 8]
                            .iter()
                            .find(|&&w| mods.first().is_some_and(|m| w > m.base && w < m.base + m.size))
                        {
                            *edges.entry((rip, ret)).or_default() += 1;
                        }
                    }
                }
                ResumeThread(h);
            }
        }
        std::thread::sleep(Duration::from_micros(500));
    }
    let wall = start.elapsed();
    eprintln!("wprof: {total} samples over {:.2?} ({} threads)", wall, open.len());

    // Symbolize with a private dbghelp session (the process is gone; load modules by path).
    let sess = 0x5150_0000usize as HANDLE;
    let mut names: HashMap<u64, String> = HashMap::new();
    unsafe {
        SymSetOptions(0x2 | 0x4 | 0x10); // UNDNAME | DEFERRED_LOADS | LOAD_LINES
        SymInitialize(sess, std::ptr::null(), 0);
        for m in &mods {
            SymLoadModuleExW(sess, std::ptr::null_mut(), m.path.as_ptr(), std::ptr::null(), m.base, m.size as u32, std::ptr::null_mut(), 0);
        }
    }
    let mut name_of = |addr: u64, lines: bool| -> String {
        if let Some(n) = names.get(&(addr | (lines as u64) << 63)) {
            return n.clone();
        }
        let module = mods.iter().find(|m| addr >= m.base && addr < m.base + m.size);
        let mut s = match module {
            Some(m) => unsafe {
                let mut si: SYMBOL_INFO = std::mem::zeroed();
                si.SizeOfStruct = 88;
                si.MaxNameLen = 1000;
                let mut disp = 0u64;
                if SymFromAddr(sess, addr, &mut disp, &mut si) != 0 {
                    let n = &si.Name[..si.NameLen.min(1000) as usize];
                    format!("{}!{}", m.name, demangle(&String::from_utf8_lossy(n)))
                } else {
                    format!("{}+{:#x}", m.name, addr - m.base)
                }
            },
            None => format!("{addr:#x}"),
        };
        if lines {
            unsafe {
                let mut li: IMAGEHLP_LINE64 = std::mem::zeroed();
                li.SizeOfStruct = std::mem::size_of::<IMAGEHLP_LINE64>() as u32;
                let mut d = 0u32;
                if SymGetLineFromAddr64(sess, addr, &mut d, &mut li) != 0 && !li.FileName.is_null() {
                    let f = std::ffi::CStr::from_ptr(li.FileName).to_string_lossy();
                    let f = f.rsplit(['\\', '/']).take(2).collect::<Vec<_>>();
                    s = format!("{}:{}", f.iter().rev().cloned().collect::<Vec<_>>().join("/"), li.LineNumber);
                }
            }
        }
        names.insert(addr | (lines as u64) << 63, s.clone());
        s
    };

    let mut by_fn: HashMap<String, u64> = HashMap::new();
    let mut by_line: HashMap<String, u64> = HashMap::new();
    let keep_idle = args[1..sep].iter().any(|a| a == "--idle");
    let mut idle = 0u64;
    for (&a, &n) in &samples {
        let f = name_of(a, false);
        if !keep_idle && is_wait(&f) {
            idle += n;
            continue;
        }
        *by_fn.entry(f).or_default() += n;
        *by_line.entry(name_of(a, true)).or_default() += n;
    }
    let total = total - idle;
    eprintln!("wprof: {idle} idle samples left out (--idle keeps them)");
    let print = |title: &str, m: &HashMap<String, u64>| {
        let mut v: Vec<_> = m.iter().collect();
        v.sort_by(|a, b| b.1.cmp(a.1));
        println!("\n== {title} (self samples, of {total}) ==");
        for (k, n) in v.into_iter().take(top) {
            println!("{:6.2}% {:7}  {k}", *n as f64 * 100.0 / total.max(1) as f64, n);
        }
    };
    print("functions", &by_fn);
    print("lines", &by_line);
    if callers {
        let mut by_edge: HashMap<(String, String), u64> = HashMap::new();
        for (&(a, r), &n) in &edges {
            *by_edge.entry((name_of(a, false), name_of(r, false))).or_default() += n;
        }
        let mut v: Vec<_> = by_edge.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        println!("\n== leaf <- nearest caller ==");
        for ((f, c), n) in v.into_iter().take(top) {
            println!("{:6.2}% {f}  <-  {c}", n as f64 * 100.0 / total.max(1) as f64);
        }
    }
}
