//! Streaming `console`: writes as it is called, replacing the engine's buffered test262
//! console. `log`/`info`/`debug` go to the out sink, `warn`/`error` to the err sink; the
//! sinks live in `OpState` so tests (and later, embedders) can capture them.

use lumen_host::OpError;
use std::io::Write;

use lumen_host::{Ctx, Extension, OpState, Value};

pub(crate) fn extension() -> Extension {
    Extension {
        name: "console",
        modules: &[
            lumen_host::namespace::<console_ns::Module>,
            lumen_host::namespace::<console_internal::Module>,
        ],
        state_init: Some(|state: &mut OpState| {
            state.put(ConsoleOut::default());
            state.put(ConsoleFlags::default());
        }),
        js_init: None,
        js_init_snapshot: None,
        lazy_globals: &[],
    }
}

/// Where `console` output goes. Default: the process's stdout/stderr.
pub struct ConsoleOut {
    pub out: Box<dyn Write>,
    pub err: Box<dyn Write>,
}

impl Default for ConsoleOut {
    fn default() -> Self {
        ConsoleOut {
            out: Box::new(std::io::stdout()),
            err: Box::new(std::io::stderr()),
        }
    }
}

/// What the native `console.*` fast path may do. It formats and writes by itself only while
/// nothing JS-visible stands between `console` and the raw sinks: the `process.stdout` /
/// `process.stderr` streams are unmaterialised (so there is no `write` to have patched), no
/// group indentation is active, and `util.inspect.defaultOptions` is untouched. Otherwise the
/// call goes to the glue's `slow` function, which owns those semantics.
#[derive(Default)]
struct ConsoleFlags {
    live: [bool; 2],
    indented: bool,
    native_off: bool,
    slow: Option<Value>,
    watch: Option<Watch>,
}

struct Watch {
    process: Value,
    getters: [Value; 2],
}

const STREAM_KEYS: [&str; 2] = ["stdout", "stderr"];

fn flags(ctx: &mut Ctx) -> &mut ConsoleFlags {
    ctx.host_mut::<ConsoleFlags>()
        .expect("console state installed")
}

/// Whether `fd`'s JS stream exists or its `process` property was replaced: console must then
/// write through it.
fn is_live(ctx: &mut Ctx, fd: usize) -> bool {
    let f = flags(ctx);
    if f.live[fd] {
        return true;
    }
    let Some(watch) = &f.watch else { return false };
    let (process, getter) = (watch.process.clone(), watch.getters[fd].clone());
    !ctx.own_getter_is(&process, STREAM_KEYS[fd], &getter)
}

fn fd_index(fd: f64) -> usize {
    (fd == 2.0) as usize
}

/// Render one argument roughly the way Node does for the common cases: strings bare, symbols
/// by description, everything else through ToString. Never throws — a value whose `toString`
/// throws prints as its typeof. (A real `util.inspect` is future work.)
fn render(ctx: &mut Ctx, v: &Value) -> String {
    match v {
        Value::Str(s) => s.to_string(),
        Value::Sym(s) => match &s.description {
            Some(d) => format!("Symbol({d})"),
            None => "Symbol()".into(),
        },
        other => match ctx.coerce_string(other) {
            Ok(s) => s.to_string(),
            Err(_) => format!("[{}]", other.type_of()),
        },
    }
}

fn write_text(ctx: &mut Ctx, to_err: bool, text: &str) {
    let mut line = lumen_host::well_formed_utf8(text).into_owned();
    line.push('\n');
    let sinks = ctx
        .host_mut::<ConsoleOut>()
        .expect("console state installed");
    let sink = if to_err {
        &mut sinks.err
    } else {
        &mut sinks.out
    };
    // A broken pipe shouldn't take the whole runtime down; console errors are swallowed.
    // Lone surrogates print as U+FFFD, like Node (see `lumen_host::well_formed_utf8`).
    let _ = sink.write_all(line.as_bytes());
    let _ = sink.flush();
}

/// Space-joined arguments, one line, to the chosen sink.
fn write_line(ctx: &mut Ctx, args: &[Value], to_err: bool) -> Result<Value, OpError> {
    let fd = to_err as usize;
    let f = flags(ctx);
    let native = !f.indented && !f.native_off;
    let slow = f.slow.clone();
    if native && !is_live(ctx, fd) {
        if let Some(text) = ctx.console_format(args) {
            write_text(ctx, to_err, &text);
            return Ok(Value::Undefined);
        }
    }
    if let Some(slow) = slow {
        let list = ctx.make_array(args.to_vec());
        return Ok(ctx.invoke(slow, Value::Undefined, &[Value::Num(fd as f64 + 1.0), list])?);
    }
    let parts: Vec<String> = args.iter().map(|a| render(ctx, a)).collect();
    write_text(ctx, to_err, &parts.join(" "));
    Ok(Value::Undefined)
}

#[lumen_bind::module(name = "console")]
mod console_ns {
    use super::*;

    #[op(name = "log")]
    fn op_log(ctx: &mut Ctx, #[varargs] args: &[Value]) -> Result<Value, OpError> {
        write_line(ctx, args, false)
    }

    #[op(name = "info")]
    fn op_info(ctx: &mut Ctx, #[varargs] args: &[Value]) -> Result<Value, OpError> {
        write_line(ctx, args, false)
    }

    #[op(name = "debug")]
    fn op_debug(ctx: &mut Ctx, #[varargs] args: &[Value]) -> Result<Value, OpError> {
        write_line(ctx, args, false)
    }

    #[op(name = "warn")]
    fn op_warn(ctx: &mut Ctx, #[varargs] args: &[Value]) -> Result<Value, OpError> {
        write_line(ctx, args, true)
    }

    #[op(name = "error")]
    fn op_error(ctx: &mut Ctx, #[varargs] args: &[Value]) -> Result<Value, OpError> {
        write_line(ctx, args, true)
    }

    /// `reportError`'s default report (after the global `onerror` declined to suppress): the same
    /// `Uncaught <error>` line on the error sink that an uncaught loop-callback error produces.
    #[op(name = "__reportUncaught")]
    fn op_report_uncaught(ctx: &mut Ctx, error: &Value) {
        let text = describe_error(ctx, error);
        write_err_line(ctx, format!("Uncaught {text}"));
    }
}

/// Glue-facing primitives; process.js moves them to `process._console` and deletes this.
#[lumen_bind::module(name = "__console")]
mod console_internal {
    use super::*;

    /// `(fd, text)` — `text` and a newline to fd 1 or 2.
    #[op(name = "write", coerce)]
    fn op_write(ctx: &mut Ctx, fd: f64, text: String) {
        write_text(ctx, fd_index(fd) == 1, &text);
    }

    /// `(...args)` — `util.format(...args)` when the native formatter covers it, else `undefined`.
    #[op(name = "format")]
    fn op_format(ctx: &mut Ctx, #[varargs] args: &[Value]) -> Option<String> {
        if flags(ctx).native_off {
            return None;
        }
        ctx.console_format(args)
    }

    #[op(name = "live", coerce)]
    fn op_live(ctx: &mut Ctx, fd: f64) -> bool {
        is_live(ctx, fd_index(fd))
    }

    #[op(name = "setLive", coerce)]
    fn op_set_live(ctx: &mut Ctx, fd: f64) {
        flags(ctx).live[fd_index(fd)] = true;
    }

    #[op(name = "indent", coerce)]
    fn op_indent(ctx: &mut Ctx, on: Option<bool>) {
        flags(ctx).indented = on.unwrap_or(false);
    }

    /// `(fn)` — the JS fallback `fn(fd, args)` for calls the native path declines.
    #[op(name = "slow")]
    fn op_slow(ctx: &mut Ctx, callback: Option<&Value>) {
        flags(ctx).slow = callback.filter(|v| v.is_callable()).cloned();
    }

    /// `(process, stdoutGetter, stderrGetter)` — the accessors that materialise the streams.
    #[op(name = "watch")]
    fn op_watch(ctx: &mut Ctx, process: &Value, out: &Value, err: &Value) {
        flags(ctx).watch = Some(Watch {
            process: process.clone(),
            getters: [out.clone(), err.clone()],
        });
    }

    #[op(name = "disableNative")]
    fn op_disable_native(ctx: &mut Ctx) {
        flags(ctx).native_off = true;
    }
}

/// [`render`] for hosts above the runtime (the REPL prints results with it).
pub fn render_value(ctx: &mut Ctx, v: &Value) -> String {
    render(ctx, v)
}

/// `"TypeError: boom"` for an Error object, else the rendered value — for uncaught reports.
pub fn describe_error(ctx: &mut Ctx, error: &Value) -> String {
    if error.as_obj().is_some() {
        let name = ctx
            .get_member(error, "name")
            .ok()
            .filter(|v| !matches!(v, Value::Undefined))
            .map(|v| render(ctx, &v));
        let message = ctx
            .get_member(error, "message")
            .ok()
            .filter(|v| !matches!(v, Value::Undefined))
            .map(|v| render(ctx, &v));
        if let Some(name) = name {
            return match message {
                Some(m) if !m.is_empty() => format!("{name}: {m}"),
                _ => name,
            };
        }
    }
    render(ctx, error)
}

/// A line straight to the err sink (uncaught-exception reports, not `console.error`).
pub(crate) fn write_err_line(ctx: &mut Ctx, line: String) {
    let sinks = ctx
        .host_mut::<ConsoleOut>()
        .expect("console state installed");
    let _ = writeln!(sinks.err, "{}", lumen_host::well_formed_utf8(&line));
    let _ = sinks.err.flush();
}
