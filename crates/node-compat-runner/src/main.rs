//! Node.js compatibility runner: runs Node's own test suite against the `lumen-cli` runtime.
//!
//! The suite is `test/` of nodejs/node, cloned on demand into `./node-test` by
//! `scripts/node-compat-clone.sh` at the Node version lumen reports as `process.version`. Every
//! file in `test/parallel` and `test/sequential` is a standalone script that `require`s
//! `test/common` and passes by exiting 0, so the runner only has to spawn the runtime per file and
//! watch the exit status. The approach and environment follow Deno's `tests/node_compat`:
//!   - `NODE_SKIP_FLAG_CHECK=1` stops `test/common` re-spawning itself with the file's
//!     `// Flags:` line; the runner forwards the flags lumen understands instead.
//!   - `NODE_TEST_KNOWN_GLOBALS=0` turns off the leaked-globals check.
//!   - `TEST_SERIAL_ID` gives every test its own `test/.tmp.<id>` scratch dir, so concurrent
//!     tests do not wipe each other's (the runner removes it afterwards).
//!   - `sequential/` runs one test at a time (shared ports); `parallel/` runs concurrently.
//!
//! A test that calls `common.skip()` (prints `1..0 # Skipped: ...`, exits 0) counts as skipped,
//! like Node's test.py does. Tests with `--expose-internals` are skipped: they `require('internal/...')` and exercise Node's
//! private modules, not the public API. `skip.txt` lists further files that cannot be run at all
//! (hang, crash the runner) with a reason.
//!
//! `passing.txt` is the checked-in list of tests expected to pass. A listed test that fails is a
//! regression and makes the run exit 1; `--update` rewrites the list from the run's results (only
//! for the tests that were selected, so a filtered run leaves the rest of the list alone).
//!
//! Usage:
//!   node-compat-runner [FILTER ...] [--jobs N] [--timeout SECS] [--update] [--verbose]
//!                      [--lumen PATH] [--suite DIR]
//! A FILTER is a substring of the test's path relative to `test/`, e.g. `parallel/test-buffer`
//! or `test-path-`. With none, all of `parallel/` and `sequential/` runs.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Directories of the suite that the runner collects; the second field is whether their tests may
/// run concurrently.
const SUITE_DIRS: &[(&str, bool)] = &[("parallel", true), ("sequential", false)];

/// Per-test output kept for the report and the failure summary.
const MAX_OUTPUT: usize = 4096;

struct Options {
    filters: Vec<String>,
    jobs: usize,
    timeout: Duration,
    update: bool,
    verbose: bool,
    lumen: PathBuf,
    suite: PathBuf,
}

#[derive(Clone)]
enum Outcome {
    Pass,
    Fail { code: Option<i32>, output: String },
    Timeout,
    Skip(String),
}

struct TestCase {
    /// Path relative to `test/`, with forward slashes: `parallel/test-buffer-alloc.js`.
    name: String,
    concurrent: bool,
}

fn main() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace = root
        .parent()
        .and_then(Path::parent)
        .expect("crate lives at <workspace>/crates/node-compat-runner")
        .to_path_buf();
    let options = parse_args(&workspace);

    let test_dir = options.suite.join("test");
    if !test_dir.join("common").is_dir() {
        die(&format!(
            "no Node test suite at {} — run scripts/node-compat-clone.sh first",
            options.suite.display()
        ));
    }
    if !options.lumen.is_file() {
        die(&format!(
            "no runtime binary at {} — build it with `cargo build --release -p lumen-cli`",
            options.lumen.display()
        ));
    }

    let passing_path = root.join("passing.txt");
    let expected = read_list(&passing_path);
    let skips: HashMap<String, String> = read_skip_list(&root.join("skip.txt"));

    let tests = collect_tests(&test_dir, &options.filters);
    if tests.is_empty() {
        die("no tests matched");
    }
    println!(
        "node-compat: running {} tests with {} ({} jobs, {}s timeout)",
        tests.len(),
        options.lumen.display(),
        options.jobs,
        options.timeout.as_secs()
    );

    let started = Instant::now();
    let mut results = run_all(&options, &test_dir, &tests, &skips);
    retry_expected_failures(&options, &test_dir, &tests, &expected, &mut results);
    let elapsed = started.elapsed();

    report(&options, &workspace, &tests, &results, &expected, elapsed);

    let regressions: Vec<&str> = tests
        .iter()
        .filter(|t| expected.contains(&t.name))
        .filter(|t| !matches!(results[&t.name], Outcome::Pass | Outcome::Skip(_)))
        .map(|t| t.name.as_str())
        .collect();

    if options.update {
        let mut updated = expected.clone();
        for test in &tests {
            match results[&test.name] {
                Outcome::Pass => updated.insert(test.name.clone()),
                _ => updated.remove(&test.name),
            };
        }
        write_list(&passing_path, &updated);
        println!(
            "updated {} ({} -> {} tests)",
            passing_path.display(),
            expected.len(),
            updated.len()
        );
    } else if !regressions.is_empty() {
        std::process::exit(1);
    }
}

fn parse_args(workspace: &Path) -> Options {
    let exe = if cfg!(windows) {
        "lumen-cli.exe"
    } else {
        "lumen-cli"
    };
    let mut options = Options {
        filters: Vec::new(),
        jobs: std::thread::available_parallelism()
            .map_or(4, |n| n.get())
            .min(16),
        timeout: Duration::from_secs(10),
        update: false,
        verbose: false,
        lumen: workspace.join("target").join("release").join(exe),
        suite: std::env::var_os("NODE_TEST")
            .map(PathBuf::from)
            .unwrap_or_else(|| workspace.join("node-test")),
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = |flag: &str| {
            args.next()
                .unwrap_or_else(|| die(&format!("{flag} expects a value")))
        };
        match arg.as_str() {
            "--jobs" | "-j" => {
                options.jobs = value(&arg)
                    .parse()
                    .unwrap_or_else(|_| die("--jobs expects a number"));
            }
            "--timeout" => {
                let secs: u64 = value(&arg)
                    .parse()
                    .unwrap_or_else(|_| die("--timeout expects seconds"));
                options.timeout = Duration::from_secs(secs);
            }
            "--lumen" => options.lumen = PathBuf::from(value(&arg)),
            "--suite" => options.suite = PathBuf::from(value(&arg)),
            "--update" => options.update = true,
            "--verbose" | "-v" => options.verbose = true,
            "-h" | "--help" => {
                println!(
                    "usage: node-compat-runner [FILTER ...] [--jobs N] [--timeout SECS] \
                     [--update] [--verbose] [--lumen PATH] [--suite DIR]"
                );
                std::process::exit(0);
            }
            a if a.starts_with('-') => die(&format!("unknown option {a}")),
            _ => options.filters.push(arg.replace('\\', "/")),
        }
    }
    options.jobs = options.jobs.max(1);
    options
}

fn collect_tests(test_dir: &Path, filters: &[String]) -> Vec<TestCase> {
    let mut tests = Vec::new();
    for &(dir, concurrent) in SUITE_DIRS {
        let Ok(entries) = std::fs::read_dir(test_dir.join(dir)) else {
            continue;
        };
        let mut names: Vec<String> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("test-") && (n.ends_with(".js") || n.ends_with(".mjs")))
            .map(|n| format!("{dir}/{n}"))
            .filter(|n| filters.is_empty() || filters.iter().any(|f| n.contains(f.as_str())))
            .collect();
        names.sort();
        tests.extend(names.into_iter().map(|name| TestCase { name, concurrent }));
    }
    tests
}

fn run_all(
    options: &Options,
    test_dir: &Path,
    tests: &[TestCase],
    skips: &HashMap<String, String>,
) -> HashMap<String, Outcome> {
    let results = Arc::new(Mutex::new(HashMap::new()));
    let done = AtomicUsize::new(0);
    let serial = AtomicUsize::new(0);
    let total = tests.len();

    let run_one = |test: &TestCase| {
        let outcome = match skips.get(&test.name) {
            Some(reason) => Outcome::Skip(reason.clone()),
            None => run_test(
                options,
                test_dir,
                test,
                serial.fetch_add(1, Ordering::Relaxed),
            ),
        };
        let n = done.fetch_add(1, Ordering::Relaxed) + 1;
        if options.verbose {
            let status = match &outcome {
                Outcome::Pass => "PASS".to_string(),
                Outcome::Fail { code, .. } => format!("FAIL ({})", exit_label(*code)),
                Outcome::Timeout => "TIMEOUT".to_string(),
                Outcome::Skip(_) => "SKIP".to_string(),
            };
            println!("[{n}/{total}] {status} {}", test.name);
        } else if n % 100 == 0 || n == total {
            print!("\r{n}/{total}");
            let _ = std::io::stdout().flush();
            if n == total {
                println!();
            }
        }
        results.lock().unwrap().insert(test.name.clone(), outcome);
    };

    let (concurrent, serial_tests): (Vec<&TestCase>, Vec<&TestCase>) =
        tests.iter().partition(|t| t.concurrent);

    let queue = Mutex::new(concurrent.into_iter());
    std::thread::scope(|scope| {
        for _ in 0..options.jobs {
            scope.spawn(|| loop {
                let next = queue.lock().unwrap().next();
                match next {
                    Some(test) => run_one(test),
                    None => break,
                }
            });
        }
    });
    for test in serial_tests {
        run_one(test);
    }

    Arc::try_unwrap(results)
        .ok()
        .expect("all workers joined")
        .into_inner()
        .unwrap()
}

/// Listed tests that failed get two more tries, one at a time, before they count as regressions:
/// a handful of child-process and timing tests fail now and then under a fully loaded machine.
fn retry_expected_failures(
    options: &Options,
    test_dir: &Path,
    tests: &[TestCase],
    expected: &BTreeSet<String>,
    results: &mut HashMap<String, Outcome>,
) {
    const RETRIES: usize = 2;
    let serial = 1_000_000;
    for (i, test) in tests
        .iter()
        .filter(|t| expected.contains(&t.name))
        .filter(|t| matches!(results[&t.name], Outcome::Fail { .. } | Outcome::Timeout))
        .collect::<Vec<_>>()
        .into_iter()
        .enumerate()
    {
        for attempt in 0..RETRIES {
            if let Outcome::Pass = run_test(options, test_dir, test, serial + i * RETRIES + attempt)
            {
                println!("flaky: {} passed on retry {}", test.name, attempt + 1);
                results.insert(test.name.clone(), Outcome::Pass);
                break;
            }
        }
    }
}

fn run_test(options: &Options, test_dir: &Path, test: &TestCase, serial_id: usize) -> Outcome {
    let path = test_dir.join(&test.name);
    let source = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => return Outcome::Skip(format!("unreadable: {e}")),
    };
    let flags = parse_flags(&source);
    if flags
        .iter()
        .any(|f| f == "--expose-internals" || f == "--expose_internals")
    {
        return Outcome::Skip("--expose-internals".to_string());
    }

    let mut command = Command::new(&options.lumen);
    command.args(flags.iter().filter(|f| forwarded_flag(f)));
    command
        .arg(&path)
        .current_dir(test_dir.parent().unwrap_or(test_dir))
        .env("NODE_SKIP_FLAG_CHECK", "1")
        .env("NODE_TEST_KNOWN_GLOBALS", "0")
        .env("NO_COLOR", "1")
        .env("TEST_SERIAL_ID", serial_id.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    tree::isolate(&mut command);

    let outcome = match command.spawn() {
        Ok(child) => {
            let tree = tree::ProcessTree::new(&child);
            wait_with_timeout(child, tree, options.timeout)
        }
        Err(e) => Outcome::Fail {
            code: None,
            output: format!("failed to spawn runtime: {e}"),
        },
    };
    let _ = std::fs::remove_dir_all(test_dir.join(format!(".tmp.{serial_id}")));
    outcome
}

fn wait_with_timeout(
    mut child: std::process::Child,
    tree: tree::ProcessTree,
    timeout: Duration,
) -> Outcome {
    // Drain both pipes on their own threads so a chatty test cannot block on a full pipe.
    let output = Arc::new(Mutex::new(Vec::new()));
    let readers: Vec<_> = [
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
    ]
    .into_iter()
    .flatten()
    .map(|mut pipe| {
        let output = Arc::clone(&output);
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            while let Ok(n) = pipe.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let mut out = output.lock().unwrap();
                if out.len() < MAX_OUTPUT * 4 {
                    out.extend_from_slice(&buf[..n]);
                }
            }
        })
    })
    .collect();

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(_) => break None,
        }
    };
    // Kill whatever the test spawned and left behind (servers, `fork`ed children, a timed-out
    // test's whole tree): stragglers would hold the pipes open and eat CPU for later tests.
    drop(tree);
    // A grandchild that escaped the tree can still hold the pipes open; don't wait on it.
    let reader_deadline = Instant::now() + Duration::from_secs(1);
    for reader in readers {
        while !reader.is_finished() && Instant::now() < reader_deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    match status {
        None => Outcome::Timeout,
        // `common.skip(reason)` prints `1..0 # Skipped: <reason>` and exits 0 — the test decided
        // it cannot run here (no crypto, no inspector, wrong platform). Node's test.py reports
        // that as a skip, not a pass.
        Some(status) if status.success() => {
            let out = output.lock().unwrap();
            let text = String::from_utf8_lossy(&out);
            match text
                .lines()
                .find_map(|l| l.trim().strip_prefix("1..0 # Skipped:"))
            {
                Some(reason) => Outcome::Skip(format!("self-skipped: {}", reason.trim())),
                None => Outcome::Pass,
            }
        }
        Some(status) => {
            let out = output.lock().unwrap();
            let text = String::from_utf8_lossy(&out);
            let text = if text.len() > MAX_OUTPUT {
                let mut end = MAX_OUTPUT;
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                format!("{} ...", &text[..end])
            } else {
                text.into_owned()
            };
            Outcome::Fail {
                code: status.code(),
                output: text,
            }
        }
    }
}

/// The flags on the file's first `// Flags:` line, as Node's test.py reads them.
fn parse_flags(source: &str) -> Vec<String> {
    source
        .lines()
        .find_map(|line| line.strip_prefix("// Flags:"))
        .map(|flags| flags.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default()
}

/// Flags lumen-cli accepts. Anything else is dropped, like Deno's runner does, rather than
/// failing the run on a V8 or Node knob lumen has no equivalent for.
fn forwarded_flag(flag: &str) -> bool {
    matches!(
        flag,
        "--expose-gc" | "--expose_gc" | "--no-warnings" | "--pending-deprecation"
    ) || flag.starts_with("--experimental-")
        || flag.starts_with("--no-experimental-")
}

/// The module a test belongs to, for the per-module score: `test-buffer-alloc.js` -> `buffer`.
fn module_of(name: &str) -> &str {
    let file = name.rsplit('/').next().unwrap_or(name);
    let stem = file.strip_prefix("test-").unwrap_or(file);
    let stem = stem.split('.').next().unwrap_or(stem);
    stem.split('-').next().unwrap_or(stem)
}

/// The line that best explains a failure: the first that names an error, else the first line.
fn first_error_line(output: &str) -> String {
    let lines = || output.lines().map(str::trim).filter(|l| !l.is_empty());
    lines()
        .find(|l| l.contains("Error") || l.contains("panicked") || l.starts_with("error"))
        .or_else(|| lines().next())
        .unwrap_or("(no output)")
        .chars()
        .take(200)
        .collect()
}

fn exit_label(code: Option<i32>) -> String {
    code.map_or("killed".to_string(), |c| format!("exit {c}"))
}

#[derive(Default)]
struct Tally {
    pass: usize,
    fail: usize,
    timeout: usize,
    skip: usize,
}

impl Tally {
    fn add(&mut self, outcome: &Outcome) {
        match outcome {
            Outcome::Pass => self.pass += 1,
            Outcome::Fail { .. } => self.fail += 1,
            Outcome::Timeout => self.timeout += 1,
            Outcome::Skip(_) => self.skip += 1,
        }
    }

    fn run(&self) -> usize {
        self.pass + self.fail + self.timeout
    }

    fn percent(&self) -> f64 {
        if self.run() == 0 {
            0.0
        } else {
            100.0 * self.pass as f64 / self.run() as f64
        }
    }
}

fn report(
    options: &Options,
    workspace: &Path,
    tests: &[TestCase],
    results: &HashMap<String, Outcome>,
    expected: &BTreeSet<String>,
    elapsed: Duration,
) {
    let mut total = Tally::default();
    let mut modules: BTreeMap<&str, Tally> = BTreeMap::new();
    for test in tests {
        let outcome = &results[&test.name];
        total.add(outcome);
        modules
            .entry(module_of(&test.name))
            .or_default()
            .add(outcome);
    }

    let mut summary = String::new();
    summary.push_str("| module | pass | run | % | timeouts | skipped |\n");
    summary.push_str("|---|---:|---:|---:|---:|---:|\n");
    for (module, t) in &modules {
        summary.push_str(&format!(
            "| {module} | {} | {} | {:.1} | {} | {} |\n",
            t.pass,
            t.run(),
            t.percent(),
            t.timeout,
            t.skip
        ));
    }

    // Failure histogram: which first error lines explain the most failures.
    let mut reasons: HashMap<String, usize> = HashMap::new();
    for test in tests {
        if let Outcome::Fail { output, .. } = &results[&test.name] {
            *reasons.entry(first_error_line(output)).or_default() += 1;
        }
    }
    let mut reasons: Vec<_> = reasons.into_iter().collect();
    reasons.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let regressions: Vec<&TestCase> = tests
        .iter()
        .filter(|t| expected.contains(&t.name))
        .filter(|t| !matches!(results[&t.name], Outcome::Pass | Outcome::Skip(_)))
        .collect();
    let newly_passing = tests
        .iter()
        .filter(|t| !expected.contains(&t.name) && matches!(results[&t.name], Outcome::Pass))
        .count();

    // Report dir: per-test results, the module table, and the failure histogram.
    let report_dir = workspace.join("node-compat-report");
    let _ = std::fs::create_dir_all(&report_dir);
    let mut tsv = String::from("test\tstatus\tdetail\n");
    for test in tests {
        let (status, detail) = match &results[&test.name] {
            Outcome::Pass => ("pass", String::new()),
            Outcome::Fail { code, output } => (
                "fail",
                format!("{}: {}", exit_label(*code), first_error_line(output)),
            ),
            Outcome::Timeout => ("timeout", String::new()),
            Outcome::Skip(reason) => ("skip", reason.clone()),
        };
        tsv.push_str(&format!(
            "{}\t{status}\t{}\n",
            test.name,
            detail.replace('\t', " ")
        ));
    }
    let _ = std::fs::write(report_dir.join("results.tsv"), tsv);
    let mut markdown = format!(
        "# Node compatibility\n\n{} of {} run tests pass ({:.1}%); {} timed out, {} skipped.\n\n{summary}\n## Top failure reasons\n\n",
        total.pass,
        total.run(),
        total.percent(),
        total.timeout,
        total.skip
    );
    for (reason, count) in reasons.iter().take(50) {
        markdown.push_str(&format!("- {count} × `{}`\n", reason.replace('`', "'")));
    }
    let _ = std::fs::write(report_dir.join("summary.md"), markdown);

    // Console summary.
    println!();
    if modules.len() > 1 {
        let width = modules.keys().map(|m| m.len()).max().unwrap_or(6).max(6);
        println!(
            "{:width$}  {:>5} / {:<5} {:>6}  {:>4}  {:>4}",
            "module", "pass", "run", "%", "t/o", "skip"
        );
        for (module, t) in &modules {
            println!(
                "{module:width$}  {:>5} / {:<5} {:>5.1}%  {:>4}  {:>4}",
                t.pass,
                t.run(),
                t.percent(),
                t.timeout,
                t.skip
            );
        }
        println!();
    }
    println!("top failure reasons:");
    for (reason, count) in reasons.iter().take(10) {
        println!("  {count:>5}  {reason}");
    }
    println!();
    println!(
        "node-compat: {} / {} pass ({:.1}%), {} timeouts, {} skipped, in {:.1}s",
        total.pass,
        total.run(),
        total.percent(),
        total.timeout,
        total.skip,
        elapsed.as_secs_f64()
    );
    println!("report: {}", report_dir.join("summary.md").display());
    if newly_passing > 0 && !options.update {
        println!("{newly_passing} tests pass that passing.txt does not list (rerun with --update)");
    }
    if !regressions.is_empty() {
        println!(
            "\n{} REGRESSIONS (listed in passing.txt, now failing):",
            regressions.len()
        );
        for test in regressions {
            let detail = match &results[&test.name] {
                Outcome::Fail { code, output } => {
                    format!("{}: {}", exit_label(*code), first_error_line(output))
                }
                Outcome::Timeout => "timed out".to_string(),
                _ => String::new(),
            };
            println!("  {}  — {detail}", test.name);
        }
    }
}

/// A list file: one test per line; `#` starts a comment.
fn read_list(path: &Path) -> BTreeSet<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// `skip.txt`: `<test>  # <reason>` per line.
fn read_skip_list(path: &Path) -> HashMap<String, String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let (test, reason) = line.split_once('#').unwrap_or((line, ""));
            let test = test.trim();
            (!test.is_empty()).then(|| (test.to_string(), reason.trim().to_string()))
        })
        .collect()
}

fn write_list(path: &Path, tests: &BTreeSet<String>) {
    let mut text = String::from(
        "# Node test-suite files (relative to node-test/test) that pass on lumen-cli.\n\
         # A listed test that fails is a regression. Regenerate with:\n\
         #   scripts/run-node-compat.sh --update\n",
    );
    for test in tests {
        text.push_str(test);
        text.push('\n');
    }
    if let Err(e) = std::fs::write(path, text) {
        die(&format!("cannot write {}: {e}", path.display()));
    }
}

fn die(message: &str) -> ! {
    eprintln!("node-compat-runner: {message}");
    std::process::exit(2);
}

/// Kills a test's whole process tree, not just the direct child: a Job Object on Windows (closing
/// the last handle kills every process in it), a process group on Unix.
mod tree {
    use std::process::{Child, Command};

    #[cfg(unix)]
    pub fn isolate(command: &mut Command) {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }

    #[cfg(unix)]
    pub struct ProcessTree(i32);

    #[cfg(unix)]
    impl ProcessTree {
        pub fn new(child: &Child) -> Self {
            ProcessTree(child.id() as i32)
        }
    }

    #[cfg(unix)]
    impl Drop for ProcessTree {
        fn drop(&mut self) {
            extern "C" {
                fn kill(pid: i32, signal: i32) -> i32;
            }
            const SIGKILL: i32 = 9;
            // The group id is the child's pid (`process_group(0)`); a negative pid targets it.
            unsafe {
                kill(-self.0, SIGKILL);
            }
        }
    }

    #[cfg(windows)]
    pub fn isolate(_command: &mut Command) {}

    #[cfg(windows)]
    pub struct ProcessTree(Option<windows::Handle>);

    #[cfg(windows)]
    impl ProcessTree {
        pub fn new(child: &Child) -> Self {
            use std::os::windows::io::AsRawHandle;
            // The child starts before it joins the job, but lumen's startup takes far longer
            // than the assignment, so it has not spawned anything of its own yet.
            ProcessTree(windows::job_for(child.as_raw_handle()))
        }
    }

    #[cfg(windows)]
    impl Drop for ProcessTree {
        fn drop(&mut self) {
            if let Some(job) = self.0.take() {
                // KILL_ON_JOB_CLOSE: closing the only handle terminates every process in the job.
                unsafe {
                    windows::CloseHandle(job);
                }
            }
        }
    }

    #[cfg(windows)]
    mod windows {
        use std::ffi::c_void;

        pub type Handle = *mut c_void;

        const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: i32 = 9;
        const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x2000;

        #[repr(C)]
        #[derive(Default)]
        struct BasicLimitInformation {
            per_process_user_time_limit: i64,
            per_job_user_time_limit: i64,
            limit_flags: u32,
            minimum_working_set_size: usize,
            maximum_working_set_size: usize,
            active_process_limit: u32,
            affinity: usize,
            priority_class: u32,
            scheduling_class: u32,
        }

        #[repr(C)]
        #[derive(Default)]
        struct ExtendedLimitInformation {
            basic: BasicLimitInformation,
            io_counters: [u64; 6],
            process_memory_limit: usize,
            job_memory_limit: usize,
            peak_process_memory_used: usize,
            peak_job_memory_used: usize,
        }

        extern "system" {
            fn CreateJobObjectW(attributes: *mut c_void, name: *const u16) -> Handle;
            fn SetInformationJobObject(
                job: Handle,
                class: i32,
                info: *const c_void,
                length: u32,
            ) -> i32;
            fn AssignProcessToJobObject(job: Handle, process: Handle) -> i32;
            pub fn CloseHandle(handle: Handle) -> i32;
        }

        /// A kill-on-close job holding `process`, or None if the OS refused (the tree then
        /// just is not cleaned up).
        pub fn job_for(process: std::os::windows::io::RawHandle) -> Option<Handle> {
            unsafe {
                let job = CreateJobObjectW(std::ptr::null_mut(), std::ptr::null());
                if job.is_null() {
                    return None;
                }
                let mut info = ExtendedLimitInformation::default();
                info.basic.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let configured = SetInformationJobObject(
                    job,
                    JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
                    &info as *const _ as *const c_void,
                    std::mem::size_of::<ExtendedLimitInformation>() as u32,
                ) != 0;
                if !configured || AssignProcessToJobObject(job, process as Handle) == 0 {
                    CloseHandle(job);
                    return None;
                }
                Some(job)
            }
        }
    }
}
