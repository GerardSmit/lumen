//! The engine side of the sampling profiler (see [`crate::cpuprof`]): turning the live call
//! stack into interned frames at a safepoint.

use super::frames::{site_pos, FnFrame, NO_SITE, SITE_PC};
use super::stack_trace::source_of;
use super::Interp;
use crate::ast::{FnSource, Function, NO_POS};
use crate::cpuprof::{Frame, Profile};
use crate::srctext::Src;
use crate::value::{Callable, Value};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

const DEFAULT_HZ: u32 = 1000;

/// Where the function's own text starts.
fn start_of(func: &Function) -> Option<(Src, u32)> {
    if let FnSource::Range { src, start, .. } = &func.source {
        return Some((src.clone(), *start));
    }
    func.lazy
        .try_borrow()
        .ok()?
        .as_ref()
        .map(|l| (l.src.clone(), l.start))
}

impl Interp {
    /// Start sampling this engine's JS stack at about `hz` samples a second. `path` is where
    /// [`Interp::write_cpu_profile`] writes the folded stacks. The sampler thread is shared by
    /// the process and keeps the rate of the first profile started.
    pub fn start_cpu_profile(&mut self, path: Option<PathBuf>, hz: u32) {
        self.cpu_prof = Some(Box::new(Profile::new(path, hz)));
    }

    /// `LUMEN_CPU_PROF=<path>` (`LUMEN_CPU_PROF_HZ`, default 1000) profiles the first engine
    /// created in the process and writes the result when that engine is dropped.
    pub(crate) fn cpuprof_auto_start(&mut self) {
        static CLAIMED: AtomicBool = AtomicBool::new(false);
        let Some(path) = std::env::var_os("LUMEN_CPU_PROF").filter(|p| !p.is_empty()) else {
            return;
        };
        if CLAIMED.swap(true, Relaxed) {
            return;
        }
        let hz = std::env::var("LUMEN_CPU_PROF_HZ")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .filter(|&h| h > 0)
            .unwrap_or(DEFAULT_HZ);
        let mut prof = Profile::new(Some(PathBuf::from(path)), hz);
        prof.auto_write = true;
        self.cpu_prof = Some(Box::new(prof));
    }

    /// Write the folded stacks and print the top-30 tables to stderr. `None` when profiling is
    /// off or no path was given.
    pub fn write_cpu_profile(&mut self) -> Option<PathBuf> {
        let prof = self.cpu_prof.as_mut()?;
        prof.auto_write = false;
        prof.write()
    }

    /// The folded stacks recorded so far (`frame;frame;frame count`, outermost first).
    pub fn cpu_profile_folded(&self) -> Option<String> {
        self.cpu_prof.as_ref().map(|p| p.folded_text())
    }

    pub(crate) fn cpuprof_flush_on_drop(&mut self) {
        if self
            .cpu_prof
            .as_ref()
            .is_some_and(|p| p.auto_write && p.has_samples())
        {
            self.write_cpu_profile();
        }
    }

    #[cold]
    #[inline(never)]
    pub(crate) fn cpuprof_sample(&mut self) {
        let Some(mut prof) = self.cpu_prof.take() else {
            return;
        };
        let ticks = crate::cpuprof::take_ticks();
        if ticks != 0 {
            self.cpuprof_record(&mut prof, ticks);
        }
        self.cpu_prof = Some(prof);
    }

    fn cpuprof_record(&self, prof: &mut Profile, ticks: u32) {
        let pending = crate::bytecode::jit::pending_frames(self);
        let total = self.fn_frames.len() + pending.len();
        prof.stack.clear();
        let mut leaf = None;
        for (k, fr) in self.fn_frames.iter().chain(pending.iter()).enumerate() {
            let id = self.cpuprof_frame(prof, fr);
            prof.stack.push(id);
            if k + 1 == total {
                leaf = Some(fr);
            }
        }
        let line = leaf.and_then(|fr| self.cpuprof_site_line(fr, self.cur_site));
        prof.record(ticks, line);
    }

    fn cpuprof_describe(&self, src: &Src, pos: u32) -> (String, Option<(u32, u32)>) {
        let Ok(mut sources) = self.sources.try_borrow_mut() else {
            return (String::new(), None);
        };
        let (file, lc, _) = sources.describe(src, pos);
        let file = match file {
            Value::Str(f) => f.to_string(),
            _ => String::new(),
        };
        (file, lc)
    }

    fn cpuprof_frame(&self, prof: &mut Profile, fr: &FnFrame) -> u32 {
        if fr.fn_ptr == 0 {
            let Some(sf) = fr.extra.as_ref().and_then(|x| x.script.as_ref()) else {
                return prof.unknown();
            };
            let key = Rc::as_ptr(sf) as usize;
            if let Some(id) = prof.lookup(key) {
                return id;
            }
            let (file, _) = self.cpuprof_describe(&sf.src, NO_POS);
            let name = if sf.eval { "(eval)" } else { "(script)" };
            let frame = Frame {
                name: name.into(),
                file,
                line: 0,
                col: 0,
            };
            return prof.intern(key, frame, Some(sf.clone() as Rc<dyn std::any::Any>));
        }
        let callee = fr.callee();
        let Ok(b) = callee.try_borrow() else {
            return prof.unknown();
        };
        let prop_name = match b
            .props
            .get("name")
            .filter(|p| !p.accessor())
            .map(|p| p.value())
        {
            Some(Value::Str(s)) if !s.is_empty() => Some(s.to_string()),
            _ => None,
        };
        let Callable::User(u) = &b.call else {
            let key = fr.fn_ptr | 1;
            if let Some(id) = prof.lookup(key) {
                return id;
            }
            let frame = Frame {
                name: prop_name.unwrap_or_else(|| "(native)".into()),
                file: String::new(),
                line: 0,
                col: 0,
            };
            return prof.intern(key, frame, None);
        };
        let key = Rc::as_ptr(&u.func) as usize;
        if let Some(id) = prof.lookup(key) {
            return id;
        }
        let func = u.func.clone();
        let name = prop_name
            .or_else(|| func.name.clone().filter(|n| !n.is_empty()))
            .unwrap_or_else(|| "(anonymous)".into());
        let (file, line, col) = match start_of(&func) {
            Some((src, start)) => {
                let (file, lc) = self.cpuprof_describe(&src, start);
                let (line, col) = lc.unwrap_or((0, 0));
                (file, line, col)
            }
            None => (String::new(), 0, 0),
        };
        let frame = Frame {
            name,
            file,
            line,
            col,
        };
        prof.intern(key, frame, Some(func as Rc<dyn std::any::Any>))
    }

    /// The line of the call `fr` last made (what a stack trace shows for it).
    fn cpuprof_site_line(&self, fr: &FnFrame, site: u32) -> Option<u32> {
        let site = site_pos(site);
        if site == NO_SITE {
            return None;
        }
        if fr.fn_ptr == 0 {
            let sf = fr.extra.as_ref()?.script.as_ref()?;
            if site & SITE_PC != 0 {
                return None;
            }
            return self.cpuprof_describe(&sf.src, site).1.map(|lc| lc.0);
        }
        let callee = fr.callee();
        let b = callee.try_borrow().ok()?;
        let Callable::User(u) = &b.call else {
            return None;
        };
        let src = source_of(&u.func)?;
        let pos = if site & SITE_PC != 0 {
            match u.func.code.get() {
                Some(Some(chunk)) => chunk.call_site_pos((site & !SITE_PC) as usize),
                _ => NO_POS,
            }
        } else {
            site
        };
        if pos == NO_POS {
            return None;
        }
        self.cpuprof_describe(&src, pos).1.map(|lc| lc.0)
    }
}
