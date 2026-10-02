//! The `lumen-py` command line: option parsing, the `--timeout` watchdog and Ctrl-C.

use crate::limits::DEFAULT_INT_MAX_STR_DIGITS;
use crate::vm::Interp;
use lumen_common::limits::Deadline;
use std::sync::atomic::{AtomicU64, Ordering};

const USAGE: &str =
    "usage: lumen-py [--timeout=MS] [--max-memory=MB] [-X int_max_str_digits=N] (<script.py> | -m module | -c command) [args]\n";

/// What runs as `__main__`.
enum Target<'a> {
    Script(&'a str),
    Module(&'a str),
    Command(&'a str),
}

/// The `--timeout` budget in ms once it ran out (0 while it has not).
static TIMED_OUT_AFTER: AtomicU64 = AtomicU64::new(0);

struct Options {
    timeout_ms: Option<u64>,
    max_memory_mb: Option<u64>,
    int_max_str_digits: usize,
    script_args: Vec<String>,
}

fn invalid_digits_limit(origin: &str) -> String {
    format!(
        "Fatal Python error: config_init_int_max_str_digits: {origin}: invalid limit; must be >= {} or 0 for unlimited.\n",
        crate::limits::INT_MAX_STR_DIGITS_THRESHOLD
    )
}

fn parse_digits_limit(text: &str, origin: &str) -> Result<usize, String> {
    match text.trim().parse::<usize>() {
        Ok(n) if n == 0 || n >= crate::limits::INT_MAX_STR_DIGITS_THRESHOLD => Ok(n),
        _ => Err(invalid_digits_limit(origin)),
    }
}

fn parse_args(args: &[String]) -> Result<Options, String> {
    let mut opts = Options { timeout_ms: None, max_memory_mb: None, int_max_str_digits: DEFAULT_INT_MAX_STR_DIGITS, script_args: Vec::new() };
    let mut digits_from_x = false;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if let Some(v) = a.strip_prefix("--timeout=") {
            opts.timeout_ms = Some(v.parse().map_err(|_| format!("lumen-py: invalid value for --timeout={v}\n"))?);
        } else if let Some(v) = a.strip_prefix("--max-memory=") {
            opts.max_memory_mb = Some(v.parse().map_err(|_| format!("lumen-py: invalid value for --max-memory={v}\n"))?);
        } else if a == "-X" || (a.starts_with("-X") && a.len() > 2) {
            let spec = if a == "-X" {
                i += 1;
                args.get(i).map(String::as_str).ok_or_else(|| "Argument expected for the -X option\n".to_string())?
            } else {
                &a[2..]
            };
            if let Some(v) = spec.strip_prefix("int_max_str_digits=") {
                opts.int_max_str_digits = parse_digits_limit(v, "-X int_max_str_digits")?;
                digits_from_x = true;
            } else if spec == "int_max_str_digits" {
                return Err(invalid_digits_limit("-X int_max_str_digits"));
            }
        } else if matches!(a, "-I" | "-E" | "-s" | "-B" | "-u" | "-q") {
        } else if a == "--" {
            i += 1;
            break;
        } else {
            break;
        }
        i += 1;
    }
    opts.script_args = args[i..].to_vec();
    if !digits_from_x {
        if let Ok(v) = std::env::var("PYTHONINTMAXSTRDIGITS") {
            opts.int_max_str_digits = parse_digits_limit(&v, "PYTHONINTMAXSTRDIGITS")?;
        }
    }
    if opts.timeout_ms.is_none() {
        opts.timeout_ms = std::env::var("LUMEN_TIMEOUT_MS").ok().and_then(|v| v.trim().parse().ok());
    }
    Ok(opts)
}

/// Runs the script named by the arguments (after any options) on the calling thread and returns
/// the process exit status. The interpreter recurses natively, so the caller needs a large
/// stack.
pub fn run_main(args: &[String]) -> i32 {
    TIMED_OUT_AFTER.store(0, Ordering::SeqCst);
    let mut it = Interp::new();
    let opts = match parse_args(args) {
        Ok(o) => o,
        Err(msg) => {
            it.write_stderr(&msg);
            return if msg.starts_with("Fatal") { 1 } else { 2 };
        }
    };
    let target = match opts.script_args.first().map(String::as_str) {
        Some(flag @ ("-m" | "-c")) => match opts.script_args.get(1) {
            Some(arg) if flag == "-m" => Target::Module(arg),
            Some(arg) => Target::Command(arg),
            None => {
                it.write_stderr(&format!("Argument expected for the {flag} option\n{USAGE}"));
                return 2;
            }
        },
        Some(path) => Target::Script(path),
        None => {
            it.write_stderr(USAGE);
            return 2;
        }
    };
    it.set_int_max_str_digits(opts.int_max_str_digits);
    if let Some(mb) = opts.max_memory_mb.filter(|&mb| mb > 0) {
        it.set_heap_limit((mb as usize).saturating_mul(1 << 20));
    }
    if let Some(ms) = opts.timeout_ms.filter(|&ms| ms > 0) {
        start_watchdog(&it, ms);
    }
    crate::builtins::signalm::install_default_handlers(&mut it);
    // `sys.path[0]` is the script's directory, or the working directory for -m and -c.
    let first_dir = match target {
        Target::Script(path) => {
            let abs = it.platform.borrow_mut().canonicalize(path);
            crate::platform::parent_dir(&abs)
        }
        _ => it.platform.borrow_mut().getcwd().unwrap_or_else(|_| ".".into()),
    };
    let mut dirs = vec![first_dir];
    if let Ok(extra) = std::env::var("PYTHONPATH") {
        dirs.extend(extra.split(':').filter(|d| !d.is_empty()).map(String::from));
    }
    it.set_path(&dirs);
    let code = match target {
        Target::Script(path) => {
            it.set_argv(&opts.script_args);
            it.run_file(path)
        }
        Target::Module(name) => {
            // runpy replaces argv[0] with the module's path once it finds it.
            let mut argv = vec!["-m".to_string()];
            argv.extend(opts.script_args[2..].iter().cloned());
            it.set_argv(&argv);
            let name = name.replace('\\', "\\\\").replace('\'', "\\'");
            it.run_source(&format!("import runpy\nrunpy._run_module_as_main('{name}')\n"), "<string>")
        }
        Target::Command(src) => {
            let mut argv = vec!["-c".to_string()];
            argv.extend(opts.script_args[2..].iter().cloned());
            it.set_argv(&argv);
            it.run_source(src, "<string>")
        }
    };
    if !it.interrupt.is_interrupted() {
        it.finalize_modules();
    }
    it.flush_out();
    let ms = TIMED_OUT_AFTER.load(Ordering::SeqCst);
    if ms != 0 && it.was_interrupted() {
        eprintln!("lumen-py: script timed out after {ms} ms");
        return 124;
    }
    code
}

/// A watchdog thread raises the interpreter's interrupt when the budget runs out.
fn start_watchdog(it: &Interp, ms: u64) {
    let handle = it.interrupt_handle();
    Deadline::start("lumen-py-timeout", std::time::Duration::from_millis(ms), move || {
        TIMED_OUT_AFTER.store(ms, Ordering::SeqCst);
        handle.interrupt();
    })
    .detach();
}

