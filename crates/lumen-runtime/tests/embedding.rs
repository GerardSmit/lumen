//! A realm embedded in a host process must not reach the host's process: `process.exit` ends the
//! realm, `process.chdir` moves only the realm's cwd, stdio are the embedder's streams, and the
//! embedder can stop a realm that loops forever or allocates without bound. Every test here runs
//! inside the test process, so a regression shows up as the test binary exiting or hanging.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lumen_runtime::{Embedding, RealmExit, Runtime, SharedWriter, Spawner};

/// Output captured from a realm.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Capture {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!(
            "lumen-embedding-{}-{name}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir.canonicalize().unwrap())
    }

    fn file(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }
}

struct Realm {
    stdin: Box<dyn std::io::Read + Send>,
    env: Vec<(String, String)>,
    cwd: Option<PathBuf>,
    live_object_limit: Option<i64>,
    spawner: Option<Arc<dyn Spawner>>,
    terminate_after: Option<Duration>,
    /// Run the script from memory under a path nothing is written to.
    in_memory: bool,
}

impl Default for Realm {
    fn default() -> Self {
        Realm {
            stdin: Box::new(std::io::empty()),
            env: Vec::new(),
            cwd: None,
            live_object_limit: None,
            spawner: None,
            terminate_after: None,
            in_memory: false,
        }
    }
}

struct Ran {
    exit: RealmExit,
    stdout: String,
    stderr: String,
}

impl Realm {
    /// Run `script` (written into `scratch`) on its own large-stack thread, as a host would.
    fn run(self, scratch: &Scratch, script: &str) -> Ran {
        let entry = if self.in_memory {
            scratch.0.join("bundle.cjs")
        } else {
            scratch.file("main.cjs", script)
        };
        let source = script.to_string();
        let in_memory = self.in_memory;
        let cwd = self.cwd.unwrap_or_else(|| scratch.0.clone());
        let (stdout, stderr) = (Capture::default(), Capture::default());
        let (out, err) = (stdout.clone(), stderr.clone());
        let (terminator_tx, terminator_rx) = std::sync::mpsc::channel();
        let Realm {
            stdin,
            env,
            live_object_limit,
            spawner,
            terminate_after,
            ..
        } = self;
        let thread = std::thread::Builder::new()
            .stack_size(256 * 1024 * 1024)
            .spawn(move || {
                let mut runtime = Runtime::new_embedded(Embedding {
                    argv: vec!["host".into(), entry.to_string_lossy().into_owned()],
                    env,
                    cwd,
                    stdin,
                    stdout: SharedWriter::new(out),
                    stderr: SharedWriter::new(err),
                    interrupt: Arc::new(AtomicBool::new(false)),
                    live_object_limit,
                    spawner,
                });
                terminator_tx.send(runtime.terminator().unwrap()).unwrap();
                if in_memory {
                    runtime.run_embedded_source(&entry.to_string_lossy(), &source)
                } else {
                    runtime.run_embedded_main(&entry.to_string_lossy())
                }
            })
            .unwrap();
        let terminator = terminator_rx.recv().unwrap();
        if let Some(after) = terminate_after {
            std::thread::sleep(after);
            terminator.terminate();
        }
        let exit = thread.join().expect("realm thread panicked");
        Ran {
            exit,
            stdout: stdout.text(),
            stderr: stderr.text(),
        }
    }
}

#[test]
fn exit_ends_the_realm_not_the_host() {
    let scratch = Scratch::new("exit");
    let ran = Realm::default().run(
        &scratch,
        "console.log('before'); process.exit(3); console.log('after');",
    );
    assert_eq!(ran.exit, RealmExit::Exited(3));
    assert_eq!(ran.stdout, "before\n");
    assert_eq!(ran.stderr, "", "the termination is not reported as an error");
}

#[test]
fn a_caught_exit_still_ends_the_realm() {
    let scratch = Scratch::new("caught-exit");
    let ran = Realm::default().run(
        &scratch,
        "try { process.exit(2); } catch (e) {}
         let n = 0;
         for (;;) { n++; }",
    );
    assert_eq!(ran.exit, RealmExit::Exited(2));
}

#[test]
fn chdir_moves_the_realm_not_the_host() {
    let scratch = Scratch::new("chdir");
    std::fs::create_dir_all(scratch.0.join("sub")).unwrap();
    scratch.file("sub/data.txt", "from sub");
    let host_cwd = std::env::current_dir().unwrap();
    let ran = Realm::default().run(
        &scratch,
        "process.chdir('sub');
         console.log(require('path').basename(process.cwd()));
         console.log(require('fs').readFileSync('data.txt', 'utf8'));",
    );
    assert_eq!(ran.exit, RealmExit::Exited(0), "{}", ran.stderr);
    assert_eq!(ran.stdout, "sub\nfrom sub\n");
    assert_eq!(std::env::current_dir().unwrap(), host_cwd);
}

#[test]
fn relative_paths_resolve_against_the_realm_cwd() {
    let scratch = Scratch::new("relative");
    scratch.file("note.txt", "realm file");
    let ran = Realm::default().run(
        &scratch,
        "const fs = require('fs');
         console.log(fs.readFileSync('note.txt', 'utf8'));
         console.log(fs.existsSync('./note.txt'));",
    );
    assert_eq!(ran.exit, RealmExit::Exited(0), "{}", ran.stderr);
    assert_eq!(ran.stdout, "realm file\ntrue\n");
}

#[test]
fn a_busy_loop_is_interruptible() {
    let scratch = Scratch::new("busy");
    let started = Instant::now();
    let ran = Realm {
        terminate_after: Some(Duration::from_millis(200)),
        ..Realm::default()
    }
    .run(&scratch, "let n = 0; while (true) { n++; }");
    assert_eq!(ran.exit, RealmExit::Terminated);
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(ran.stderr, "");
}

#[test]
fn a_caught_termination_does_not_keep_running() {
    let scratch = Scratch::new("catch-loop");
    let ran = Realm {
        terminate_after: Some(Duration::from_millis(200)),
        ..Realm::default()
    }
    .run(
        &scratch,
        "function spin() { let n = 0; while (true) { n++; } }
         while (true) { try { spin(); } catch (e) {} }",
    );
    assert_eq!(ran.exit, RealmExit::Terminated);
}

#[test]
fn a_realm_blocked_on_a_timer_wakes_to_terminate() {
    let scratch = Scratch::new("timer");
    let started = Instant::now();
    let ran = Realm {
        terminate_after: Some(Duration::from_millis(100)),
        ..Realm::default()
    }
    .run(&scratch, "setTimeout(() => console.log('late'), 60_000);");
    assert_eq!(ran.exit, RealmExit::Terminated);
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(ran.stdout, "");
}

#[test]
fn the_live_object_ceiling_ends_the_realm() {
    let scratch = Scratch::new("heap");
    let ran = Realm {
        live_object_limit: Some(200_000),
        ..Realm::default()
    }
    .run(
        &scratch,
        "const keep = [];
         try { for (;;) keep.push({ n: keep.length }); } catch (e) { console.log('caught'); }
         for (;;) keep.push({});",
    );
    assert_eq!(ran.exit, RealmExit::HeapLimit);
}

#[test]
fn stdin_and_env_come_from_the_embedder() {
    let scratch = Scratch::new("stdio");
    let ran = Realm {
        stdin: Box::new(std::io::Cursor::new(b"ping".to_vec())),
        env: vec![("REALM_ONLY".into(), "yes".into())],
        ..Realm::default()
    }
    .run(
        &scratch,
        "console.log(process.env.REALM_ONLY, process.env.PATH === undefined);
         process.stdin.on('data', (chunk) => process.stdout.write('got ' + chunk.toString() + '\\n'));",
    );
    assert_eq!(ran.exit, RealmExit::Exited(0), "{}", ran.stderr);
    assert_eq!(ran.stdout, "yes true\ngot ping\n");
}

#[test]
fn a_program_runs_from_memory_and_requires_beside_its_path() {
    let scratch = Scratch::new("in-memory");
    scratch.file("helper.cjs", "module.exports = 'helped';");
    let ran = Realm {
        in_memory: true,
        ..Realm::default()
    }
    .run(
        &scratch,
        "const path = require('path');
         console.log(require('./helper.cjs'), path.basename(__filename), require.main === module);",
    );
    assert_eq!(ran.exit, RealmExit::Exited(0), "{}", ran.stderr);
    assert_eq!(ran.stdout, "helped bundle.cjs true
");
    assert!(!scratch.0.join("bundle.cjs").exists());
}

#[test]
fn file_descriptors_0_to_2_are_the_realms_stdio() {
    let scratch = Scratch::new("stdfd");
    let ran = Realm {
        stdin: Box::new(std::io::Cursor::new(b"pong".to_vec())),
        ..Realm::default()
    }
    .run(
        &scratch,
        "const fs = require('fs');
         const buf = Buffer.alloc(8);
         const n = fs.readSync(0, buf, 0, 8, null);
         fs.writeSync(1, 'read ' + buf.subarray(0, n).toString() + '\\n');
         fs.writeSync(2, 'to stderr\\n');
         const fd = fs.openSync('file.txt', 'w');
         console.log('opened above stdio', fd > 2);
         fs.closeSync(fd);",
    );
    assert_eq!(ran.exit, RealmExit::Exited(0), "{}", ran.stderr);
    assert_eq!(ran.stdout, "read pong\nopened above stdio true\n");
    assert_eq!(ran.stderr, "to stderr\n");
}

struct CountingSpawner(AtomicUsize);

impl Spawner for CountingSpawner {
    fn spawn(&self, command: &mut std::process::Command) -> std::io::Result<std::process::Child> {
        self.0.fetch_add(1, Ordering::SeqCst);
        command.spawn()
    }
}

#[test]
fn subprocesses_start_through_the_spawner_in_the_realm_cwd() {
    let scratch = Scratch::new("spawn");
    let spawner = Arc::new(CountingSpawner(AtomicUsize::new(0)));
    let (shell, flag, print_cwd) = if cfg!(windows) {
        ("cmd.exe", "/c", "cd")
    } else {
        ("/bin/sh", "-c", "pwd")
    };
    let script = format!(
        "const {{ spawn }} = require('child_process');
         const child = spawn({shell:?}, [{flag:?}, {print_cwd:?}]);
         let out = '';
         child.stdout.on('data', (d) => out += d.toString());
         child.on('close', (code) => console.log(code, out.trim()));"
    );
    let ran = Realm {
        spawner: Some(spawner.clone()),
        // The child needs a PATH to find its shell on some systems.
        env: std::env::vars().collect(),
        ..Realm::default()
    }
    .run(&scratch, &script);
    assert_eq!(ran.exit, RealmExit::Exited(0), "{}", ran.stderr);
    assert_eq!(spawner.0.load(Ordering::SeqCst), 1);
    let printed = ran.stdout.trim().strip_prefix("0 ").unwrap_or("").to_string();
    assert_eq!(
        Path::new(&printed).canonicalize().ok(),
        Some(scratch.0.clone()),
        "stdout: {}",
        ran.stdout
    );
}
