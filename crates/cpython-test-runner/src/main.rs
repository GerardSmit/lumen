//! Runs CPython's `Lib/test/test_*.py` files against the `lumen-py` binary.
//!
//! Every test file runs in its own process (so a crash or hang costs one file), in parallel, with
//! a wall-clock timeout. `unittest -v` output is parsed for per-file pass/fail/error/skip counts;
//! a file that produces no results (import failure, crash, timeout) counts as one file-level error.
//!
//! Usage:
//!   cpython-test-runner [FILTER ...]   run files whose name contains any FILTER
//!   --root DIR       CPython checkout (default: ./cpython, from scripts/cpython-fetch.sh)
//!   --bin PATH       interpreter (default: target/release/lumen-py)
//!   --jobs N         parallel processes (default: CPU count)
//!   --timeout SECS   per-file budget (default: 60)
//!   --report DIR     output directory (default: cpython-test-report)
//!   --skip FILE      files to skip (default: crates/cpython-test-runner/skip.txt)
//!   --baseline FILE  expected results (default: crates/cpython-test-runner/baseline.txt)
//!   --check          exit 1 if any file regressed against the baseline
//!   --bless          rewrite the baseline (and summary.txt beside it) from this run

mod parse;

use parse::{classify, parse_counts, Counts, Status};
use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

struct Options {
    root: PathBuf,
    bin: PathBuf,
    jobs: usize,
    timeout: Duration,
    report: PathBuf,
    skip: PathBuf,
    baseline: PathBuf,
    check: bool,
    bless: bool,
    filters: Vec<String>,
}

struct FileResult {
    name: String,
    status: Status,
    counts: Counts,
    secs: f64,
}

fn parse_args() -> Result<Options, String> {
    let mut o = Options {
        root: PathBuf::from("cpython"),
        bin: PathBuf::from("target/release/lumen-py"),
        jobs: thread::available_parallelism().map(|n| n.get()).unwrap_or(4),
        timeout: Duration::from_secs(60),
        report: PathBuf::from("cpython-test-report"),
        skip: PathBuf::from("crates/cpython-test-runner/skip.txt"),
        baseline: PathBuf::from("crates/cpython-test-runner/baseline.txt"),
        check: false,
        bless: false,
        filters: Vec::new(),
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut value = |flag: &str| args.next().ok_or_else(|| format!("{flag} needs a value"));
        match a.as_str() {
            "--root" => o.root = PathBuf::from(value("--root")?),
            "--bin" => o.bin = PathBuf::from(value("--bin")?),
            "--jobs" => o.jobs = value("--jobs")?.parse().map_err(|_| "--jobs needs a number".to_string())?,
            "--timeout" => o.timeout = Duration::from_secs(value("--timeout")?.parse().map_err(|_| "--timeout needs a number".to_string())?),
            "--report" => o.report = PathBuf::from(value("--report")?),
            "--skip" => o.skip = PathBuf::from(value("--skip")?),
            "--baseline" => o.baseline = PathBuf::from(value("--baseline")?),
            "--check" => o.check = true,
            "--bless" => o.bless = true,
            "-h" | "--help" => return Err(String::new()),
            f if f.starts_with("--") => return Err(format!("unknown option {f}")),
            f => o.filters.push(f.to_string()),
        }
    }
    o.jobs = o.jobs.max(1);
    Ok(o)
}

fn read_names(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter(|l| !l.is_empty())
        .map(|l| l.trim_end_matches(".py").to_string())
        .collect()
}

fn discover(test_dir: &Path, filters: &[String]) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(test_dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| n.starts_with("test_") && n.ends_with(".py"))
                .map(|n| n.trim_end_matches(".py").to_string())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    if !filters.is_empty() {
        names.retain(|n| filters.iter().any(|f| n.contains(f.as_str())));
    }
    names
}

fn run_one(o: &Options, name: &str, log_dir: &Path) -> FileResult {
    let test_dir = o.root.join("Lib").join("test");
    let log_path = log_dir.join(format!("{name}.log"));
    let started = Instant::now();
    let log = match fs::File::create(&log_path).and_then(|f| f.try_clone().map(|g| (f, g))) {
        Ok(pair) => pair,
        Err(e) => return failure(name, Status::Crash(format!("cannot create log: {e}")), started),
    };
    let lib = fs::canonicalize(o.root.join("Lib")).unwrap_or_else(|_| o.root.join("Lib"));
    let spawned = Command::new(&o.bin)
        .arg(format!("{name}.py"))
        .arg("-v")
        .current_dir(&test_dir)
        .env("PYTHONPATH", &lib)
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.0))
        .stderr(Stdio::from(log.1))
        .spawn();
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => return failure(name, Status::Crash(format!("cannot spawn {}: {e}", o.bin.display())), started),
    };
    let exit = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if started.elapsed() > o.timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return failure(name, Status::Timeout, started);
            }
            Ok(None) => thread::sleep(Duration::from_millis(10)),
            Err(_) => break None,
        }
    };
    let bytes = fs::read(&log_path).unwrap_or_default();
    let output = String::from_utf8_lossy(&bytes).into_owned();
    let code = exit.and_then(|s| s.code());
    let counts = parse_counts(&output);
    let status = classify(counts, code, &output);
    FileResult { name: name.to_string(), status, counts, secs: started.elapsed().as_secs_f64() }
}

fn failure(name: &str, status: Status, started: Instant) -> FileResult {
    FileResult { name: name.to_string(), status, counts: Counts::default(), secs: started.elapsed().as_secs_f64() }
}

fn run_all(o: &Arc<Options>, names: Vec<String>, log_dir: &Path) -> Vec<FileResult> {
    let queue = Arc::new(Mutex::new(names.into_iter().collect::<VecDeque<_>>()));
    let results = Arc::new(Mutex::new(Vec::new()));
    let mut handles = Vec::new();
    for _ in 0..o.jobs {
        let (queue, results, o) = (queue.clone(), results.clone(), o.clone());
        let log_dir = log_dir.to_path_buf();
        handles.push(thread::spawn(move || loop {
            let next = queue.lock().ok().and_then(|mut q| q.pop_front());
            let Some(name) = next else { break };
            let r = run_one(&o, &name, &log_dir);
            if let Ok(mut rs) = results.lock() {
                rs.push(r);
            }
        }));
    }
    for h in handles {
        let _ = h.join();
    }
    let mut out = match results.lock() {
        Ok(mut guard) => std::mem::take(&mut *guard),
        Err(_) => Vec::new(),
    };
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn status_detail(s: &Status) -> String {
    match s {
        Status::ImportError(r) | Status::Crash(r) => r.clone(),
        _ => String::new(),
    }
}

fn json_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn write_report(o: &Options, results: &[FileResult], skipped: &[String]) -> std::io::Result<String> {
    let mut totals = Counts::default();
    let mut by_status: BTreeMap<&'static str, u32> = BTreeMap::new();
    let mut reasons: BTreeMap<String, u32> = BTreeMap::new();
    for r in results {
        totals.pass += r.counts.pass;
        totals.fail += r.counts.fail;
        totals.error += r.counts.error;
        totals.skip += r.counts.skip;
        *by_status.entry(r.status.label()).or_default() += 1;
        if let Status::ImportError(reason) | Status::Crash(reason) = &r.status {
            *reasons.entry(reason.clone()).or_default() += 1;
        }
    }
    let file_errors = results.iter().filter(|r| matches!(r.status, Status::ImportError(_) | Status::Timeout | Status::Crash(_))).count();

    let mut tsv = String::from("file\tstatus\tpass\tfail\terror\tskip\tseconds\tdetail\n");
    let mut json_files = Vec::new();
    for r in results {
        tsv.push_str(&format!("{}\t{}\t{}\t{}\t{}\t{}\t{:.2}\t{}\n", r.name, r.status.label(), r.counts.pass, r.counts.fail, r.counts.error, r.counts.skip, r.secs, status_detail(&r.status)));
        json_files.push(format!(
            "    {}: {{ \"status\": {}, \"pass\": {}, \"fail\": {}, \"error\": {}, \"skip\": {}, \"seconds\": {:.2}, \"detail\": {} }}",
            json_string(&r.name),
            json_string(r.status.label()),
            r.counts.pass,
            r.counts.fail,
            r.counts.error,
            r.counts.skip,
            r.secs,
            json_string(&status_detail(&r.status)),
        ));
    }
    fs::write(o.report.join("files.tsv"), tsv)?;

    let mut text = String::new();
    text.push_str(&format!("files run: {}  skipped (skip.txt): {}\n", results.len(), skipped.len()));
    for (label, n) in &by_status {
        text.push_str(&format!("  {label:<13} {n}\n"));
    }
    text.push_str(&format!("file-level errors: {file_errors}\n"));
    text.push_str(&format!("tests: pass {}  fail {}  error {}  skip {}\n", totals.pass, totals.fail, totals.error, totals.skip));
    let mut ranked: Vec<(&String, &u32)> = reasons.iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    if !ranked.is_empty() {
        text.push_str("\ntop file-level failure reasons:\n");
        for (reason, n) in ranked.iter().take(25) {
            text.push_str(&format!("  {n:>4}  {reason}\n"));
        }
    }
    fs::write(o.report.join("summary.txt"), &text)?;

    let json = format!(
        "{{\n  \"files_run\": {},\n  \"files_skipped\": {},\n  \"file_errors\": {},\n  \"tests\": {{ \"pass\": {}, \"fail\": {}, \"error\": {}, \"skip\": {} }},\n  \"files\": {{\n{}\n  }}\n}}\n",
        results.len(),
        skipped.len(),
        file_errors,
        totals.pass,
        totals.fail,
        totals.error,
        totals.skip,
        json_files.join(",\n"),
    );
    fs::write(o.report.join("summary.json"), json)?;
    Ok(text)
}

/// One baseline line: `name<TAB>status<TAB>pass<TAB>fail<TAB>error<TAB>skip`.
fn read_baseline(path: &Path) -> BTreeMap<String, (String, u32)> {
    let mut map = BTreeMap::new();
    for line in fs::read_to_string(path).unwrap_or_default().lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() >= 3 && !line.starts_with('#') {
            map.insert(parts[0].to_string(), (parts[1].to_string(), parts[2].parse().unwrap_or(0)));
        }
    }
    map
}

/// Writes the per-file baseline and, next to it, `summary.txt` (the run's totals), so the
/// committed files show the score without a local report directory.
fn write_baseline(path: &Path, results: &[FileResult], summary: &str) -> std::io::Result<()> {
    let mut out = String::from("# file\tstatus\tpass\tfail\terror\tskip  (regenerate with cpython-test-runner --bless)\n");
    for r in results {
        let c = &r.counts;
        out.push_str(&format!("{}\t{}\t{}\t{}\t{}\t{}\n", r.name, r.status.label(), c.pass, c.fail, c.error, c.skip));
    }
    fs::write(path, out)?;
    fs::write(path.with_file_name("summary.txt"), summary)
}

fn regressions(baseline: &BTreeMap<String, (String, u32)>, results: &[FileResult]) -> Vec<String> {
    let mut out = Vec::new();
    for r in results {
        let Some((status, pass)) = baseline.get(&r.name) else { continue };
        let was_clean = status == "ok" || status == "failed";
        if !was_clean {
            continue;
        }
        let broke = matches!(r.status, Status::ImportError(_) | Status::Timeout | Status::Crash(_));
        if broke {
            out.push(format!("{}: was {status} ({pass} passing), now {} {}", r.name, r.status.label(), status_detail(&r.status)));
        } else if r.counts.pass < *pass {
            out.push(format!("{}: passing tests dropped {pass} -> {}", r.name, r.counts.pass));
        } else if status == "ok" && r.status == Status::Failed {
            out.push(format!("{}: was ok, now failing", r.name));
        }
    }
    out
}

fn main() {
    let o = match parse_args() {
        Ok(o) => Arc::new(o),
        Err(msg) => {
            if !msg.is_empty() {
                eprintln!("cpython-test-runner: {msg}");
            }
            eprintln!("usage: cpython-test-runner [--root DIR] [--bin PATH] [--jobs N] [--timeout SECS] [--report DIR] [--skip FILE] [--baseline FILE] [--check] [--bless] [FILTER ...]");
            std::process::exit(2);
        }
    };
    let test_dir = o.root.join("Lib").join("test");
    if !test_dir.is_dir() {
        eprintln!("cpython-test-runner: {} not found; run scripts/cpython-fetch.sh first", test_dir.display());
        std::process::exit(2);
    }
    if !o.bin.is_file() {
        eprintln!("cpython-test-runner: interpreter {} not found; run `cargo build --release -p lumen-py`", o.bin.display());
        std::process::exit(2);
    }
    let log_dir = o.report.join("logs");
    if let Err(e) = fs::create_dir_all(&log_dir) {
        eprintln!("cpython-test-runner: cannot create {}: {e}", log_dir.display());
        std::process::exit(2);
    }
    let skip = read_names(&o.skip);
    let (names, skipped): (Vec<String>, Vec<String>) = discover(&test_dir, &o.filters).into_iter().partition(|n| !skip.contains(n));
    let bin = match fs::canonicalize(&o.bin) {
        Ok(b) => b,
        Err(_) => o.bin.clone(),
    };
    let o = Arc::new(Options {
        root: o.root.clone(),
        bin,
        jobs: o.jobs,
        timeout: o.timeout,
        report: o.report.clone(),
        skip: o.skip.clone(),
        baseline: o.baseline.clone(),
        check: o.check,
        bless: o.bless,
        filters: o.filters.clone(),
    });
    eprintln!("running {} files with {} jobs ({} skipped)", names.len(), o.jobs, skipped.len());
    let results = run_all(&o, names, &log_dir);
    let summary = match write_report(&o, &results, &skipped) {
        Ok(text) => {
            print!("{text}");
            text
        }
        Err(e) => {
            eprintln!("cpython-test-runner: cannot write report: {e}");
            std::process::exit(2);
        }
    };
    println!("\nreport: {}", o.report.display());
    if o.bless {
        match write_baseline(&o.baseline, &results, &summary) {
            Ok(()) => println!("baseline written: {}", o.baseline.display()),
            Err(e) => eprintln!("cpython-test-runner: cannot write baseline: {e}"),
        }
    }
    if o.check {
        let found = regressions(&read_baseline(&o.baseline), &results);
        if found.is_empty() {
            println!("no regressions against {}", o.baseline.display());
        } else {
            println!("\nREGRESSIONS ({}):", found.len());
            for f in &found {
                println!("  {f}");
            }
            std::process::exit(1);
        }
    }
}
