//! A JS-level sampling CPU profiler.
//!
//! ## Design
//! A sampler thread ticks at the configured rate. A tick either lands in a *phase* the engine
//! marked with [`phase`] (`(gc)`, `(parse)`, `(compile)`, `(jit-compile)`, `(idle)`, `(lazy-parse)`), where the
//! engine thread cannot sample itself, and is counted there directly, or bumps the thread's
//! pending tick count. The
//! engine thread polls that counter at the safepoints that exist in every tier
//! (`Interp::gc_check_amortized`: function entry and bytecode backedges; `Interp::gc_check`:
//! tree-walker loop turns and the JIT loop safepoint) and, when it is non-zero, records the
//! current JS stack (`Interp::fn_frames` plus the JIT's pending direct-call records, as stack
//! traces read them) into a per-engine [`Profile`]. Nothing is read from another thread.
//!
//! While the profiler is on, JIT loops reach their safepoint every [`JIT_BUDGET`] turns instead
//! of every `SAFEPOINT_BUDGET`; the full collector check still runs only on every 16th of them.
//!
//! Disabled cost: one relaxed load of a global flag at those safepoints.
//!
//! ## Limits
//! Samples land at safepoints, so time is credited to the stack of the next safepoint. A callee
//! without loops that JIT code calls directly, or a callee the JIT inlined, has no safepoint of
//! its own: its time shows under the nearest enclosing function that has one. A gap of more
//! than [`MAX_WEIGHT`] ticks (a long native call, a blocking wait) is credited as `MAX_WEIGHT`.
//! Ticks and phases are per thread: engines profiled on different threads do not mix.

use std::collections::HashMap;
use std::path::PathBuf;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering::Relaxed};
use std::sync::{Arc, Mutex, Weak};

/// What the engine thread is doing when it cannot take a sample itself.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Phase {
    Gc = 1,
    Parse = 2,
    Compile = 3,
    JitCompile = 4,
    Idle = 5,
    LazyParse = 6,
}

const PHASE_NAMES: [&str; 7] = [
    "",
    "(gc)",
    "(parse)",
    "(compile)",
    "(jit-compile)",
    "(idle)",
    "(lazy-parse)",
];

/// JIT loop turns between safepoints while profiling (the low byte must stay 0, see `JitFrame`).
pub(crate) const JIT_BUDGET: i64 = 1 << 8;
const _: () = assert!(JIT_BUDGET & 0xff == 0);

const MAX_WEIGHT: u32 = 16;

static ENABLED: AtomicBool = AtomicBool::new(false);
static SAMPLER: std::sync::Once = std::sync::Once::new();
static SLOTS: Mutex<Vec<Weak<Slot>>> = Mutex::new(Vec::new());

/// The sampler's view of one profiled engine thread.
#[derive(Default)]
pub(crate) struct Slot {
    ticks: AtomicU32,
    phase: AtomicU8,
    phase_ticks: [AtomicU64; 7],
}

thread_local! {
    static SLOT: RefCell<Option<Arc<Slot>>> = const { RefCell::new(None) };
}

fn with_slot<R>(f: impl FnOnce(&Slot) -> R) -> Option<R> {
    SLOT.try_with(|s| s.borrow().as_deref().map(f)).ok().flatten()
}

/// The current thread's slot, registered with the sampler on first use.
fn thread_slot() -> Arc<Slot> {
    SLOT.with(|s| {
        s.borrow_mut()
            .get_or_insert_with(|| {
                let slot = Arc::new(Slot::default());
                let mut slots = SLOTS.lock().unwrap_or_else(|e| e.into_inner());
                slots.retain(|w| w.strong_count() > 0);
                slots.push(Arc::downgrade(&slot));
                slot
            })
            .clone()
    })
}

/// Whether a profiler was ever started in this process.
#[inline(always)]
pub(crate) fn enabled() -> bool {
    ENABLED.load(Relaxed)
}

/// Whether the sampler asked this thread for a sample.
#[inline(always)]
pub(crate) fn pending() -> bool {
    enabled() && slot_pending()
}

// Out of line so the thread-local lookup stays behind the `enabled` test on call hot paths.
#[inline(never)]
fn slot_pending() -> bool {
    with_slot(|s| s.ticks.load(Relaxed) != 0).unwrap_or(false)
}

pub(crate) fn take_ticks() -> u32 {
    with_slot(|s| s.ticks.swap(0, Relaxed))
        .unwrap_or(0)
        .min(MAX_WEIGHT)
}

/// Marks the engine thread as busy in `phase` until dropped; its ticks are counted under the
/// phase's pseudo-frame. A no-op unless a profiler is running on this thread.
pub struct PhaseGuard(Option<u8>);

pub fn phase(phase: Phase) -> PhaseGuard {
    if !enabled() {
        return PhaseGuard(None);
    }
    PhaseGuard(with_slot(|s| s.phase.swap(phase as u8, Relaxed)))
}

impl Drop for PhaseGuard {
    fn drop(&mut self) {
        if let Some(prev) = self.0 {
            with_slot(|s| s.phase.store(prev, Relaxed));
        }
    }
}

fn start_sampler(hz: u32) {
    SAMPLER.call_once(|| {
        let period = std::time::Duration::from_nanos(1_000_000_000 / hz.clamp(1, 20_000) as u64);
        let spawned = std::thread::Builder::new()
            .name("lumen-cpuprof".into())
            .spawn(move || {
                let mut next = std::time::Instant::now() + period;
                loop {
                    let now = std::time::Instant::now();
                    if next > now {
                        std::thread::sleep(next - now);
                    }
                    next += period;
                    let now = std::time::Instant::now();
                    if next < now {
                        next = now + period;
                    }
                    let slots = SLOTS.lock().unwrap_or_else(|e| e.into_inner());
                    for slot in slots.iter().filter_map(Weak::upgrade) {
                        match slot.phase.load(Relaxed) {
                            0 => {
                                slot.ticks.fetch_add(1, Relaxed);
                            }
                            p => {
                                slot.phase_ticks[p as usize].fetch_add(1, Relaxed);
                            }
                        }
                    }
                }
            });
        if spawned.is_ok() {
            ENABLED.store(true, Relaxed);
        }
    });
}

/// One interned function (or script) frame.
pub(crate) struct Frame {
    pub name: String,
    pub file: String,
    pub line: u32,
    pub col: u32,
}

impl Frame {
    fn label(&self) -> String {
        let name: String = self
            .name
            .chars()
            .map(|c| if c == ';' || c.is_control() { ' ' } else { c })
            .collect();
        match (self.file.is_empty(), self.line) {
            (true, _) => name,
            (false, 0) => format!("{name} ({})", self.file),
            (false, l) => format!("{name} ({}:{l}:{})", self.file, self.col),
        }
    }
}

/// The samples of one engine.
pub(crate) struct Profile {
    pub(crate) path: Option<PathBuf>,
    pub(crate) auto_write: bool,
    hz: u32,
    slot: Arc<Slot>,
    frames: Vec<Frame>,
    by_key: HashMap<usize, u32>,
    pins: Vec<Rc<dyn std::any::Any>>,
    pub(crate) stack: Vec<u32>,
    folded: HashMap<Vec<u32>, u64>,
    lines: HashMap<(u32, u32), u64>,
    samples: u64,
}

impl Profile {
    pub(crate) fn new(path: Option<PathBuf>, hz: u32) -> Profile {
        start_sampler(hz);
        let slot = thread_slot();
        slot.ticks.store(0, Relaxed);
        for t in &slot.phase_ticks {
            t.store(0, Relaxed);
        }
        let mut p = Profile {
            path,
            auto_write: false,
            hz,
            slot,
            frames: Vec::new(),
            by_key: HashMap::new(),
            pins: Vec::new(),
            stack: Vec::new(),
            folded: HashMap::new(),
            lines: HashMap::new(),
            samples: 0,
        };
        p.frames.push(Frame {
            name: "(root)".into(),
            file: String::new(),
            line: 0,
            col: 0,
        });
        p.frames.push(Frame {
            name: "(unknown)".into(),
            file: String::new(),
            line: 0,
            col: 0,
        });
        p
    }

    pub(crate) fn unknown(&self) -> u32 {
        1
    }

    pub(crate) fn lookup(&self, key: usize) -> Option<u32> {
        self.by_key.get(&key).copied()
    }

    /// Intern `frame` under `key`; `pin` keeps whatever `key` is the address of alive, so the
    /// address cannot be reused by another function while the profile exists.
    pub(crate) fn intern(&mut self, key: usize, frame: Frame, pin: Option<Rc<dyn std::any::Any>>) -> u32 {
        let id = self.frames.len() as u32;
        self.frames.push(frame);
        self.by_key.insert(key, id);
        self.pins.extend(pin);
        id
    }

    /// Record `ticks` samples of the stack in `self.stack` (outermost first).
    pub(crate) fn record(&mut self, ticks: u32, leaf_line: Option<u32>) {
        let ticks = ticks as u64;
        self.samples += ticks;
        if self.stack.is_empty() {
            self.stack.push(0);
        }
        match self.folded.get_mut(self.stack.as_slice()) {
            Some(c) => *c += ticks,
            None => {
                self.folded.insert(self.stack.clone(), ticks);
            }
        }
        if let (Some(line), Some(&leaf)) = (leaf_line, self.stack.last()) {
            *self.lines.entry((leaf, line)).or_default() += ticks;
        }
    }

    pub(crate) fn has_samples(&self) -> bool {
        self.samples > 0 || self.slot.phase_ticks.iter().any(|t| t.load(Relaxed) > 0)
    }

    fn label_stacks(&self) -> Vec<(Vec<String>, u64)> {
        let mut out: Vec<(Vec<String>, u64)> = self
            .folded
            .iter()
            .map(|(s, &n)| {
                (
                    s.iter().map(|&id| self.frames[id as usize].label()).collect(),
                    n,
                )
            })
            .collect();
        for (p, t) in self.slot.phase_ticks.iter().enumerate().skip(1) {
            let n = t.load(Relaxed);
            if n > 0 {
                out.push((vec![PHASE_NAMES[p].to_string()], n));
            }
        }
        out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        out
    }

    /// Folded stacks, `frame;frame;frame count`, outermost frame first.
    pub(crate) fn folded_text(&self) -> String {
        let mut s = String::new();
        for (stack, n) in self.label_stacks() {
            s.push_str(&stack.join(";"));
            s.push(' ');
            s.push_str(&n.to_string());
            s.push('\n');
        }
        s
    }

    /// `count<TAB>function<TAB>line` for the innermost frame's last known line.
    pub(crate) fn lines_text(&self) -> String {
        let mut rows: Vec<(String, u32, u64)> = self
            .lines
            .iter()
            .map(|(&(f, l), &n)| (self.frames[f as usize].label(), l, n))
            .collect();
        rows.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)).then(a.1.cmp(&b.1)));
        let mut s = String::new();
        for (label, line, n) in rows {
            s.push_str(&format!("{n}\t{label}\t{line}\n"));
        }
        s
    }

    /// The top `limit` functions by self and by total samples.
    pub(crate) fn report(&self, limit: usize) -> String {
        let stacks = self.label_stacks();
        let all: u64 = stacks.iter().map(|s| s.1).sum();
        let mut own: HashMap<&str, u64> = HashMap::new();
        let mut total: HashMap<&str, u64> = HashMap::new();
        for (stack, n) in &stacks {
            if let Some(leaf) = stack.last() {
                *own.entry(leaf.as_str()).or_default() += n;
            }
            let mut seen: Vec<&str> = Vec::new();
            for f in stack {
                if !seen.contains(&f.as_str()) {
                    seen.push(f.as_str());
                    *total.entry(f.as_str()).or_default() += n;
                }
            }
        }
        let table = |title: &str, m: &HashMap<&str, u64>| {
            let mut rows: Vec<(&str, u64)> = m.iter().map(|(k, v)| (*k, *v)).collect();
            rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
            let mut s = format!("[cpuprof] top {limit} by {title}\n");
            for (label, n) in rows.into_iter().take(limit) {
                let pct = if all == 0 { 0.0 } else { n as f64 * 100.0 / all as f64 };
                s.push_str(&format!("  {pct:5.1}% {n:8}  {label}\n"));
            }
            s
        };
        format!(
            "[cpuprof] {all} samples at {} Hz\n{}{}",
            self.hz,
            table("self", &own),
            table("total", &total)
        )
    }

    /// Write the folded stacks to the profile's path (and the line table to `<path>.lines`),
    /// then print the report to stderr.
    pub(crate) fn write(&self) -> Option<PathBuf> {
        let path = self.path.clone()?;
        let mut lines_path = path.clone().into_os_string();
        lines_path.push(".lines");
        let ok = std::fs::write(&path, self.folded_text()).is_ok()
            && std::fs::write(&lines_path, self.lines_text()).is_ok();
        eprint!("{}", self.report(30));
        if ok {
            eprintln!("[cpuprof] wrote {}", path.display());
            Some(path)
        } else {
            eprintln!("[cpuprof] could not write {}", path.display());
            None
        }
    }
}
