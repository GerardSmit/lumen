//! `RuntimeSession`: the Node/Web runtime (`lumen-runtime`) in a browser. One realm on the
//! single-threaded loop; the page drives it. Nothing blocks: `eval` runs everything that is ready
//! and returns, and the page resumes the realm by calling `tick` when the reported timer is due or
//! by pushing the result of a browser operation (`pushEvent`) it started on the realm's behalf.
//!
//! The page supplies a *host object* whose methods the runtime's ops call: `fetch`, `wsOpen`,
//! `wsSend`, `wsClose` (all start work and return) and `syncCall` (blocks until answered, see
//! `js/bridge.js`).

use std::io::Write;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use js_sys::{Array, Object, Reflect, Uint8Array};
use lumen_host::browser::{self, Arg, Event};
use lumen_host::{CompletionSender, TaskId, TaskRegistry};
use lumen_os::vfs::{self, FileSystem};
use lumen_runtime::{Completion, Embedding, LoopStatus, Runtime, SharedWriter};
use wasm_bindgen::prelude::*;

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Capture {
    fn take(&self) -> String {
        let mut bytes = self.0.lock().unwrap_or_else(|e| e.into_inner());
        String::from_utf8_lossy(&std::mem::take(&mut *bytes)).into_owned()
    }
}

impl Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A file tree the page serves through `syncCall("fs.*")`.
///
/// `fs.stat` answers `[0]` (missing) or `[1 file | 2 dir, size as u64 LE]`; `fs.list` answers
/// lines of `<d|f>\t<size>\t<name>`; `fs.read` answers `[0]` (missing) or `[1, ...bytes]`.
struct Remote {
    prefix: String,
}

impl Remote {
    fn relative(&self, path: &str) -> String {
        path.strip_prefix(self.prefix.trim_end_matches('/'))
            .unwrap_or(path)
            .to_string()
    }
}

impl vfs::Backend for Remote {
    fn stat(&self, path: &str) -> Option<vfs::RemoteStat> {
        let out = browser::sync_call("fs.stat", self.relative(path).as_bytes()).ok()?;
        let (kind, rest) = out.split_first()?;
        let size = u64::from_le_bytes(rest.get(..8)?.try_into().ok()?);
        match kind {
            1 => Some(vfs::RemoteStat {
                is_dir: false,
                size,
            }),
            2 => Some(vfs::RemoteStat { is_dir: true, size }),
            _ => None,
        }
    }

    fn read(&self, path: &str) -> Option<Vec<u8>> {
        let out = browser::sync_call("fs.read", self.relative(path).as_bytes()).ok()?;
        match out.split_first() {
            Some((1, bytes)) => Some(bytes.to_vec()),
            _ => None,
        }
    }

    fn list(&self, path: &str) -> Option<Vec<vfs::RemoteEntry>> {
        let out = browser::sync_call("fs.list", self.relative(path).as_bytes()).ok()?;
        let text = String::from_utf8(out).ok()?;
        let mut entries = Vec::new();
        for line in text.lines() {
            let mut parts = line.splitn(3, '\t');
            let (Some(kind), Some(size), Some(name)) = (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            entries.push(vfs::RemoteEntry {
                name: name.to_string(),
                is_dir: kind == "d",
                size: size.parse().unwrap_or(0),
            });
        }
        Some(entries)
    }
}

fn get(obj: &JsValue, key: &str) -> JsValue {
    if obj.is_undefined() || obj.is_null() {
        return JsValue::UNDEFINED;
    }
    Reflect::get(obj, &key.into()).unwrap_or(JsValue::UNDEFINED)
}

fn to_arg(v: &JsValue) -> Arg {
    if v.is_null() || v.is_undefined() {
        Arg::Null
    } else if let Some(b) = v.as_bool() {
        Arg::from(b)
    } else if let Some(n) = v.as_f64() {
        Arg::from(n)
    } else if let Some(s) = v.as_string() {
        Arg::from(s)
    } else if let Some(a) = v.dyn_ref::<Uint8Array>() {
        Arg::from(a.to_vec())
    } else if let Some(b) = v.dyn_ref::<js_sys::ArrayBuffer>() {
        Arg::from(Uint8Array::new(b).to_vec())
    } else {
        Arg::from(format!("{v:?}"))
    }
}

/// One realm of the runtime, persistent across calls.
#[wasm_bindgen]
pub struct RuntimeSession {
    rt: Runtime,
    sender: CompletionSender,
    stdout: Capture,
    stderr: Capture,
    ended: Vec<TaskId>,
}

#[wasm_bindgen]
impl RuntimeSession {
    /// `host` is the object described in the module docs. `options` (all optional):
    /// `{ cwd: string, argv: string[], env: { [name]: string } }`.
    #[wasm_bindgen(constructor)]
    pub fn new(host: JsValue, options: JsValue) -> RuntimeSession {
        browser::set_host(host);
        lumen_host::time::install_engine_clock();
        let cwd = get(&options, "cwd")
            .as_string()
            .unwrap_or_else(|| "/".to_string());
        let _ = vfs::mem().mkdir(&cwd, 0o755, true);
        let _ = vfs::mem().chdir(&cwd);
        let argv: Vec<String> = match get(&options, "argv").dyn_ref::<Array>() {
            Some(list) => list.iter().filter_map(|v| v.as_string()).collect(),
            None => vec!["lumen".to_string()],
        };
        let mut env = Vec::new();
        let env_obj = get(&options, "env");
        if env_obj.is_object() {
            for key in Object::keys(env_obj.unchecked_ref::<Object>()).iter() {
                if let (Some(k), Some(v)) = (
                    key.as_string(),
                    get(&env_obj, &key.as_string().unwrap_or_default()).as_string(),
                ) {
                    env.push((k, v));
                }
            }
        }
        let stdout = Capture::default();
        let stderr = Capture::default();
        let rt = Runtime::new_embedded(Embedding {
            argv,
            env,
            cwd: std::path::PathBuf::from(cwd),
            stdin: Box::new(std::io::empty()),
            stdout: SharedWriter::new(stdout.clone()),
            stderr: SharedWriter::new(stderr.clone()),
            interrupt: Arc::new(AtomicBool::new(false)),
            live_object_limit: None,
            spawner: None,
        });
        let sender = rt.completion_sender();
        RuntimeSession {
            rt,
            sender,
            stdout,
            stderr,
            ended: Vec::new(),
        }
    }

    /// Run `src` as a script, then every turn that is ready. The result is
    /// `{ ok, value?, error?, stdout, stderr, status }`.
    pub fn eval(&mut self, src: &str) -> JsValue {
        let outcome = match self.rt.eval(src) {
            Ok(Completion::Value(v)) => Ok(Some(v)),
            Ok(Completion::Throw { name, message }) => Err(if name.is_empty() {
                format!("Uncaught: {message}")
            } else {
                format!("Uncaught {name}: {message}")
            }),
            Err(e) => Err(format!("SyntaxError (line {}): {}", e.line, e.message)),
        };
        self.report(outcome)
    }

    /// Run `src` as an ES module identified by `key` (relative imports resolve against it).
    #[wasm_bindgen(js_name = evalModule)]
    pub fn eval_module(&mut self, src: &str, key: &str) -> JsValue {
        let outcome = self.rt.run_module_source(src, key).map(|_| None);
        self.report(outcome)
    }

    /// Run the CommonJS file at `path` (in the virtual file system) as the program entry.
    #[wasm_bindgen(js_name = runMain)]
    pub fn run_main(&mut self, path: &str) -> JsValue {
        let outcome = self.rt.run_main(path).map(|_| None);
        self.report(outcome)
    }

    /// Run what is ready now. Call it when `status.nextTimerMs` has elapsed.
    pub fn tick(&mut self) -> JsValue {
        self.report(Ok(None))
    }

    /// Deliver the outcome of a browser operation the runtime started: `task` is the id the host
    /// method received, `kind` and `args` the event (see the `fetch` and `ws*` host methods).
    /// Runs the loop and returns what `tick` does.
    #[wasm_bindgen(js_name = pushEvent)]
    pub fn push_event(&mut self, task: f64, kind: &str, args: Array) -> JsValue {
        let id = task as TaskId;
        if matches!(kind, "close" | "fail" | "error" | "io") {
            self.ended.push(id);
        }
        let event = Event::new(kind, args.iter().map(|v| to_arg(&v)).collect());
        self.sender.send(id, Box::new(event));
        self.report(Ok(None))
    }

    /// Create or replace a file, making missing parent directories.
    #[wasm_bindgen(js_name = writeFile)]
    pub fn write_file(&mut self, path: &str, data: &[u8]) -> Result<(), JsValue> {
        if let Some((dir, _)) = path.rsplit_once('/') {
            if !dir.is_empty() {
                let _ = vfs::mem().mkdir(dir, 0o755, true);
            }
        }
        use lumen_os::fs::flags::{O_CREAT, O_TRUNC, O_WRONLY};
        vfs::mem()
            .write_file(path, data, O_WRONLY | O_CREAT | O_TRUNC, 0o666)
            .map_err(|e| JsValue::from_str(&format!("{}: {path}", e.code())))
    }

    #[wasm_bindgen(js_name = readFile)]
    pub fn read_file(&mut self, path: &str) -> Result<Vec<u8>, JsValue> {
        vfs::mem()
            .read_file(path, 0)
            .map_err(|e| JsValue::from_str(&format!("{}: {path}", e.code())))
    }

    /// Serve the paths under `prefix` through the host's `syncCall("fs.*")` (see [`Remote`]).
    #[wasm_bindgen(js_name = mountRemote)]
    pub fn mount_remote(&mut self, prefix: &str) {
        vfs::mem().mount(
            prefix,
            Arc::new(Remote {
                prefix: prefix.to_string(),
            }),
        );
    }
}

impl RuntimeSession {
    fn report(&mut self, outcome: Result<Option<String>, String>) -> JsValue {
        let status = self.rt.run_until_idle();
        for id in std::mem::take(&mut self.ended) {
            if let Some(registry) = self.rt.engine().ctx().host_mut::<TaskRegistry>() {
                registry.cancel(id);
            }
        }
        let out = Object::new();
        let set = |k: &str, v: &JsValue| {
            let _ = Reflect::set(&out, &JsValue::from_str(k), v);
        };
        match outcome {
            Ok(value) => {
                set("ok", &JsValue::TRUE);
                if let Some(v) = value {
                    set("value", &JsValue::from_str(&v));
                }
            }
            Err(error) => {
                set("ok", &JsValue::FALSE);
                set("error", &JsValue::from_str(&error));
            }
        }
        set("stdout", &JsValue::from_str(&self.stdout.take()));
        set("stderr", &JsValue::from_str(&self.stderr.take()));
        set("status", &status_object(status));
        out.into()
    }
}

fn status_object(status: LoopStatus) -> JsValue {
    let o = Object::new();
    let _ = Reflect::set(
        &o,
        &"nextTimerMs".into(),
        &status
            .next_timer_ms
            .map_or(JsValue::NULL, JsValue::from_f64),
    );
    let _ = Reflect::set(
        &o,
        &"pendingTasks".into(),
        &JsValue::from_bool(status.pending_tasks),
    );
    let _ = Reflect::set(&o, &"idle".into(), &JsValue::from_bool(status.idle));
    let _ = Reflect::set(&o, &"halted".into(), &JsValue::from_bool(status.halted));
    o.into()
}
