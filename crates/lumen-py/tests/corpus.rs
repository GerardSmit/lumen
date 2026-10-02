//! End-to-end corpus: runs every `tests/py/**/X.py` entry script through the
//! `lumen-py` binary and compares stdout (and, when `X.err` exists, the exit code
//! and last stderr line) with the committed expected files.
//!
//! Every failing script must be listed in `tests/py/expected-failures.txt` as
//! `path  # reason`, one per line; a listed script without a reason fails the run.
//!
//! Env: `LUMEN_PY_CORPUS_FILTER=substr` runs a subset;
//! `LUMEN_PY_CORPUS_BLESS=1` rewrites `tests/py/expected-failures.txt` (existing reasons are
//! kept; new entries get the failure description).

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(10);

fn corpus_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("py")
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = rd.filter_map(Result::ok).collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('_') || name.starts_with('.') {
            continue;
        }
        let path = e.path();
        if path.is_dir() {
            collect(&path, out);
        } else if path.extension().is_some_and(|x| x == "py") {
            out.push(path);
        }
    }
}

struct Outcome {
    stdout: Vec<u8>,
    last_stderr: String,
    code: Option<i32>,
    timed_out: bool,
}

fn drain<R: Read + Send + 'static>(mut r: R) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = r.read_to_end(&mut buf);
        buf
    })
}

fn run_script(script: &Path) -> Result<Outcome, String> {
    let dir = script.parent().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_lumen-py"))
        .arg(script.file_name().unwrap())
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn failed: {e}"))?;
    let out_h = drain(child.stdout.take().unwrap());
    let err_h = drain(child.stderr.take().unwrap());
    let status = lumen_os::child::wait_timeout(&mut child, TIMEOUT, Duration::from_millis(5))
        .map_err(|e| format!("wait failed: {e}"))?;
    let timed_out = status.is_none();
    let stdout = out_h.join().unwrap_or_default();
    let stderr = err_h.join().unwrap_or_default();
    let stderr = String::from_utf8_lossy(&stderr);
    let last_stderr = stderr.trim_end_matches('\n').lines().next_back().unwrap_or("").to_string();
    Ok(Outcome { stdout, last_stderr, code: status.and_then(|s| s.code()), timed_out })
}

fn first_diff(expected: &[u8], actual: &[u8]) -> String {
    let e = String::from_utf8_lossy(expected);
    let a = String::from_utf8_lossy(actual);
    let mut el = e.split('\n');
    let mut al = a.split('\n');
    let mut n = 1;
    loop {
        match (el.next(), al.next()) {
            (None, None) => return "(no difference)".into(),
            (x, y) if x == y => n += 1,
            (x, y) => {
                return format!(
                    "line {n}: expected {:?}, got {:?}",
                    x.unwrap_or("<eof>"),
                    y.unwrap_or("<eof>")
                );
            }
        }
    }
}

/// `None` when the script passes, otherwise a one-line failure description.
fn check(script: &Path) -> Option<String> {
    let out = match run_script(script) {
        Ok(o) => o,
        Err(e) => return Some(e),
    };
    if out.timed_out {
        return Some(format!("timed out after {}s", TIMEOUT.as_secs()));
    }
    let expected_out = fs::read(script.with_extension("out")).unwrap_or_default();
    let err_text = fs::read_to_string(script.with_extension("err")).unwrap_or_default();
    let expected_err = if err_text.trim().is_empty() {
        None
    } else {
        let mut lines = err_text.lines();
        let code: i32 = lines.next().and_then(|l| l.trim().parse().ok()).unwrap_or(1);
        Some((code, lines.next().unwrap_or("").to_string()))
    };
    if out.stdout != expected_out {
        return Some(format!("stdout differs, {}", first_diff(&expected_out, &out.stdout)));
    }
    match expected_err {
        Some((code, last)) => {
            if out.code != Some(code) {
                return Some(format!("exit code: expected {code}, got {:?}", out.code));
            }
            if out.last_stderr != last {
                return Some(format!(
                    "last stderr line: expected {last:?}, got {:?}",
                    out.last_stderr
                ));
            }
        }
        None => {
            if out.code != Some(0) {
                return Some(format!(
                    "exit code: expected 0, got {:?} (stderr: {:?})",
                    out.code, out.last_stderr
                ));
            }
        }
    }
    None
}

fn rel(root: &Path, p: &Path) -> String {
    p.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/")
}

/// `path -> reason` from `expected-failures.txt`; lines without a reason are returned separately.
fn read_baseline(path: &Path) -> (BTreeMap<String, String>, Vec<String>) {
    let mut map = BTreeMap::new();
    let mut missing = Vec::new();
    for line in fs::read_to_string(path).unwrap_or_default().lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (p, reason) = line.split_once('#').unwrap_or((line, ""));
        let (p, reason) = (p.trim().to_string(), reason.trim().to_string());
        if reason.is_empty() {
            missing.push(p.clone());
        }
        map.insert(p, reason);
    }
    (map, missing)
}

#[test]
fn corpus() {
    let root = corpus_root();
    let filter = std::env::var("LUMEN_PY_CORPUS_FILTER").ok();
    let bless = std::env::var("LUMEN_PY_CORPUS_BLESS").is_ok_and(|v| v == "1");

    let mut scripts = Vec::new();
    collect(&root, &mut scripts);
    if let Some(f) = &filter {
        scripts.retain(|p| rel(&root, p).contains(f.as_str()));
    }

    let next = Arc::new(AtomicUsize::new(0));
    let results: Arc<Mutex<Vec<(String, Option<String>)>>> = Arc::new(Mutex::new(Vec::new()));
    let scripts = Arc::new(scripts);
    let workers = thread::available_parallelism().map_or(4, |n| n.get()).min(16);
    let handles: Vec<_> = (0..workers)
        .map(|_| {
            let (next, results, scripts, root) =
                (next.clone(), results.clone(), scripts.clone(), root.clone());
            thread::spawn(move || loop {
                let i = next.fetch_add(1, Ordering::SeqCst);
                let Some(script) = scripts.get(i) else { break };
                let r = check(script);
                results.lock().unwrap().push((rel(&root, script), r));
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }

    let mut results = results.lock().unwrap().clone();
    results.sort();

    let mut per_dir: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for (path, r) in &results {
        let dir = path.rsplit_once('/').map_or(".", |(d, _)| d).to_string();
        let e = per_dir.entry(dir).or_default();
        e.1 += 1;
        if r.is_none() {
            e.0 += 1;
        }
    }
    println!();
    for (dir, (p, t)) in &per_dir {
        println!("{dir:<24} {p}/{t}");
    }
    let total = results.len();
    let passed = results.iter().filter(|(_, r)| r.is_none()).count();
    println!("overall: {passed}/{total}");

    let baseline_path = root.join("expected-failures.txt");
    let (baseline, missing_reasons) = read_baseline(&baseline_path);
    let allowed = |path: &str| baseline.contains_key(path);

    let failing: Vec<&(String, Option<String>)> = results.iter().filter(|(_, r)| r.is_some()).collect();
    for (path, r) in &failing {
        let tag = if allowed(path) { "expected-fail" } else { "FAIL" };
        println!("{tag}: {path}: {}", r.as_deref().unwrap_or(""));
    }
    for (path, r) in &results {
        if r.is_none() && baseline.contains_key(path) {
            println!("fixed: remove from expected-failures: {path}");
        }
    }

    if bless {
        if filter.is_some() {
            panic!("LUMEN_PY_CORPUS_BLESS=1 cannot be combined with LUMEN_PY_CORPUS_FILTER");
        }
        let mut text = String::from(
            "# Corpus scripts that are known to fail: `path  # reason`, one per line.\n",
        );
        for (path, r) in &failing {
            let reason = baseline
                .get(path.as_str())
                .filter(|r| !r.is_empty())
                .cloned()
                .unwrap_or_else(|| r.clone().unwrap_or_default().replace('\n', " "));
            text.push_str(&format!("{path}  # {reason}\n"));
        }
        fs::write(&baseline_path, text).expect("write expected-failures.txt");
        println!("blessed {} expected failures", failing.len());
        return;
    }

    assert!(
        missing_reasons.is_empty(),
        "expected-failures.txt entries need a `# reason`:\n{}",
        missing_reasons.join("\n")
    );
    let unexpected: Vec<&str> =
        failing.iter().map(|(p, _)| p.as_str()).filter(|p| !allowed(p)).collect();
    assert!(
        unexpected.is_empty(),
        "{} corpus file(s) failed unexpectedly:\n{}",
        unexpected.len(),
        unexpected.join("\n")
    );
}
