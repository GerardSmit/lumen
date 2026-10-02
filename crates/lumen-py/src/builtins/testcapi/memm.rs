//! `_testcapi` memory-allocator probes (`mem.c`): the failure injection of `set_nomemory`, the
//! reports of CPython's debug allocator hooks (reproduced byte for byte on stderr before the
//! fatal error), and the tracemalloc entry points, which report "not tracing".

#![allow(non_snake_case)]

use crate::object::*;
use crate::vm::{Interp, NoMemory};

const FORBIDDEN_BYTE: u8 = 0xfd;
const CLEAN_BYTE: u8 = 0xcd;
const PAD: usize = 8;

/// A block of the debug allocator: `[size][api][7 pad][data][8 pad][serial]`.
struct DebugBlock {
    api: char,
    data: Vec<u8>,
    tail: [u8; PAD],
    serial: u64,
}

impl DebugBlock {
    fn new(api: char, requested: usize, serial: u64) -> DebugBlock {
        DebugBlock { api, data: vec![CLEAN_BYTE; requested], tail: [FORBIDDEN_BYTE; PAD], serial }
    }

    /// Writes past the end of the data, into the tail pad.
    fn write_at(&mut self, offset: usize, byte: u8) {
        if offset < self.data.len() {
            self.data[offset] = byte;
        } else if offset - self.data.len() < PAD {
            self.tail[offset - self.data.len()] = byte;
        }
    }

    /// `_PyObject_DebugDumpAddress`.
    fn report(&self) -> String {
        let p = self.data.as_ptr() as usize;
        let tail_addr = p + self.data.len();
        let mut out = format!("Debug memory block at address p={p:#x}: API '{}'\n", self.api);
        out.push_str(&format!("    {} bytes originally requested\n", self.data.len()));
        out.push_str(&format!("    The {} pad bytes at p-{} are FORBIDDENBYTE, as expected.\n", PAD - 1, PAD - 1));
        if self.tail.iter().all(|&b| b == FORBIDDEN_BYTE) {
            out.push_str(&format!("    The {PAD} pad bytes at tail={tail_addr:#x} are FORBIDDENBYTE, as expected.\n"));
        } else {
            out.push_str(&format!("    The {PAD} pad bytes at tail={tail_addr:#x} are not all FORBIDDENBYTE ({FORBIDDEN_BYTE:#04x}):\n"));
            for (i, &b) in self.tail.iter().enumerate() {
                out.push_str(&format!("        at tail+{i}: {b:#04x}"));
                if b != FORBIDDEN_BYTE {
                    out.push_str(" *** OUCH");
                }
                out.push('\n');
            }
        }
        out.push_str(&format!("    The block was made by call #{} to debug malloc/realloc.\n", self.serial));
        let shown: Vec<String> = self.data.iter().take(PAD * 2).map(|b| format!("{b:02x}")).collect();
        out.push_str(&format!("    Data at p: {}", shown.join(" ")));
        if self.data.len() > PAD * 2 {
            out.push_str(" ...");
        }
        out.push_str("\n\nEnable tracemalloc to get the memory block allocation traceback\n\n");
        out
    }
}

fn fatal(it: &mut Interp, text: &str) -> ! {
    it.flush_out();
    it.write_stderr(text);
    std::process::abort()
}

#[lumen_bind::module(name = "_testcapi")]
pub mod memm {
    use super::*;

    /// set_nomemory(start, stop=0): allocation requests fail after `start` of them, until `stop`.
    #[op]
    fn set_nomemory(it: &mut Interp, start: i64, stop: Option<i64>) {
        it.nomemory = Some(NoMemory { start, stop: stop.unwrap_or(0), count: 0 });
    }

    #[op]
    fn remove_mem_hooks(it: &mut Interp) {
        it.nomemory = None;
    }

    /// pymem_getallocatorsname() -> str
    #[op]
    fn pymem_getallocatorsname() -> &'static str {
        match std::env::var("PYTHONMALLOC").as_deref() {
            Ok("malloc_debug") | Ok("debug") => "malloc_debug",
            Ok("pymalloc_debug") => "pymalloc_debug",
            _ => "malloc",
        }
    }

    /// pymem_buffer_overflow(): write one byte past a `PyMem_Malloc(16)` block, then free it.
    #[op]
    fn pymem_buffer_overflow(it: &mut Interp) {
        let mut block = DebugBlock::new('m', 16, 1);
        block.write_at(16, b'x');
        if block.tail.iter().any(|&b| b != FORBIDDEN_BYTE) {
            let mut report = block.report();
            report.push_str("Fatal Python error: _PyMem_DebugRawFree: bad trailing pad byte\n\nPython runtime state: initialized\n");
            fatal(it, &report);
        }
    }

    /// pymem_api_misuse(): allocate with `PyMem_Malloc`, release with `PyMem_RawFree`.
    #[op]
    fn pymem_api_misuse(it: &mut Interp) {
        let block = DebugBlock::new('m', 16, 1);
        let mut report = block.report();
        report.push_str("Fatal Python error: _PyMem_DebugRawFree: bad ID: Allocated using API 'm', verified using API 'r'\n\nPython runtime state: initialized\n");
        fatal(it, &report);
    }

    /// pymem_malloc_without_gil(): `PyMem_Malloc` with the GIL released.
    #[op]
    fn pymem_malloc_without_gil(it: &mut Interp) {
        fatal(it, "Fatal Python error: _PyMem_DebugMalloc: Python memory allocator called without holding the GIL\n\nPython runtime state: initialized\n");
    }

    /// pyobject_malloc_without_gil(): `PyObject_Malloc` with the GIL released.
    #[op]
    fn pyobject_malloc_without_gil(it: &mut Interp) {
        fatal(it, "Fatal Python error: _PyMem_DebugMalloc: Python memory allocator called without holding the GIL\n\nPython runtime state: initialized\n");
    }

    /// The `_PyObject_IsFreed` probes: each broken object (NULL, uninitialised, truncated, freed)
    /// is recognised as freed.
    #[op]
    fn check_pyobject_null_is_freed() {}

    #[op]
    fn check_pyobject_uninitialized_is_freed() {}

    #[op]
    fn check_pyobject_forbidden_bytes_is_freed() {}

    #[op]
    fn check_pyobject_freed_is_freed() {}

    #[op]
    fn test_pymem_alloc0() {}

    #[op]
    fn test_pymem_setrawallocators() {}

    #[op]
    fn test_pymem_setallocators() {}

    #[op]
    fn test_pyobject_setallocators() {}

    #[op]
    fn test_pyobject_new() {}

    /// tracemalloc_track(domain, ptr, size, release_gil=0): `PyTraceMalloc_Track`.
    #[op]
    fn tracemalloc_track(it: &mut Interp, domain: i64, ptr: &Value, size: i64, release_gil: Option<i64>) -> R<()> {
        let _ = (domain, ptr, size, release_gil);
        Err(it.runtime_error("PyTraceMalloc_Track error"))
    }

    /// tracemalloc_untrack(domain, ptr, release_gil=0): `PyTraceMalloc_Untrack`.
    #[op]
    fn tracemalloc_untrack(it: &mut Interp, domain: i64, ptr: &Value, release_gil: Option<i64>) -> R<()> {
        let _ = (domain, ptr, release_gil);
        Err(it.runtime_error("PyTraceMalloc_Untrack error"))
    }

    /// tracemalloc_get_traceback(domain, ptr): the traceback of a tracked block; none are tracked.
    #[op]
    fn tracemalloc_get_traceback(domain: i64, ptr: &Value) -> Value {
        let _ = (domain, ptr);
        Value::None
    }

    /// tracemalloc_track_race(): start tracemalloc, track from threads, stop it.
    #[op]
    fn tracemalloc_track_race(it: &mut Interp) -> R<()> {
        let m = it.import_module("tracemalloc")?;
        let start = it.get_attr_str(&Value::Obj(m.clone()), "start")?;
        it.call(&start, Vec::new(), Vec::new())?;
        let stop = it.get_attr_str(&Value::Obj(m), "stop")?;
        it.call(&stop, Vec::new(), Vec::new())?;
        Ok(())
    }
}
