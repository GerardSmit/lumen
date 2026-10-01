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
        let dir =
            std::env::temp_dir().join(format!("lumen-embedding-{}-{name}", std::process::id()));
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

#[test]
fn cjs_dependency_in_module_package_preserves_callable_exports() {
    let scratch = Scratch::new("cjs-module-package");
    scratch.file("package.json", r#"{"type":"module"}"#);
    scratch.file(
        "factory.cjs",
        "function factory() { return 'loaded'; } module.exports = factory; module.exports.create = factory;",
    );
    let ran = Realm::default().run(
        &scratch,
        "const factory = require('./factory.cjs'); console.log(typeof factory, factory === factory.create, factory.create());",
    );
    assert_eq!(ran.exit, RealmExit::Exited(0), "{}", ran.stderr);
    assert_eq!(ran.stdout, "function true loaded\n");
    std::fs::remove_dir_all(&scratch.0).unwrap();
}

#[test]
fn esm_package_type_is_not_shadowed_by_repository_metadata() {
    let scratch = Scratch::new("esm-package-metadata");
    scratch.file(
        "package.json",
        r#"{"repository":{"type":"git"},"type":"module"}"#,
    );
    scratch.file("helper.js", "export const value = 42;");
    scratch.file(
        "entry.mjs",
        "import { value } from './helper.js'; console.log(value);",
    );
    let ran = Realm::default().run(&scratch, "import('./entry.mjs');");
    assert_eq!(ran.exit, RealmExit::Exited(0), "{}", ran.stderr);
    assert_eq!(ran.stdout, "42\n");
    std::fs::remove_dir_all(&scratch.0).unwrap();
}

#[test]
fn esm_private_imports_use_own_package_and_node_condition() {
    let scratch = Scratch::new("esm-private-imports");
    scratch.file("package.json", r##"{"type":"module","imports":{"#plain":"./plain.js","#conditional":{"node":"./node.js","default":"./browser.js"}}}"##);
    scratch.file("plain.js", "export const plain = 'plain';");
    scratch.file("node.js", "export const platform = 'node';");
    scratch.file("browser.js", "throw new Error('wrong condition');");
    scratch.file("entry.mjs", "import { plain } from '#plain'; import { platform } from '#conditional'; console.log(plain, platform);");
    let ran = Realm::default().run(&scratch, "import('./entry.mjs');");
    assert_eq!(ran.exit, RealmExit::Exited(0), "{}", ran.stderr);
    assert_eq!(ran.stdout, "plain node\n");
    std::fs::remove_dir_all(&scratch.0).unwrap();
}

#[test]
fn esm_blocked_root_exports_do_not_load_main_or_index() {
    let scratch = Scratch::new("esm-blocked-root");
    std::fs::create_dir_all(scratch.0.join("node_modules/blocked")).unwrap();
    scratch.file(
        "node_modules/blocked/package.json",
        r#"{"type":"module","exports":{".":null},"main":"./legacy.js"}"#,
    );
    scratch.file(
        "node_modules/blocked/legacy.js",
        "throw new Error('legacy main bypassed exports');",
    );
    scratch.file(
        "node_modules/blocked/index.js",
        "throw new Error('legacy index bypassed exports');",
    );
    scratch.file("entry.mjs", "try { await import('blocked'); console.log('unexpected import'); } catch (error) { console.log(error.name); }");
    let ran = Realm::default().run(&scratch, "import('./entry.mjs');");
    assert_eq!(ran.exit, RealmExit::Exited(0), "{}", ran.stderr);
    assert_eq!(ran.stdout, "TypeError\n");
    std::fs::remove_dir_all(&scratch.0).unwrap();
}

#[test]
fn esm_export_arrays_choose_first_target_without_missing_file_fallback() {
    let scratch = Scratch::new("esm-export-arrays");
    for name in ["ordered", "missing"] {
        std::fs::create_dir_all(scratch.0.join("node_modules").join(name)).unwrap();
    }
    scratch.file("node_modules/ordered/package.json", r#"{"type":"module","exports":[{"require":"./wrong.cjs"},null,{"import":"./first.js"},"./second.js"]}"#);
    scratch.file("node_modules/ordered/first.js", "export default 'first';");
    scratch.file(
        "node_modules/ordered/second.js",
        "throw new Error('second array target executed');",
    );
    scratch.file(
        "node_modules/missing/package.json",
        r#"{"type":"module","exports":["./absent.js","./fallback.js"]}"#,
    );
    scratch.file(
        "node_modules/missing/fallback.js",
        "throw new Error('missing file triggered array fallback');",
    );
    scratch.file("entry.mjs", "import value from 'ordered'; console.log(value); try { await import('missing'); console.log('unexpected import'); } catch (error) { console.log(error.name); }");
    let ran = Realm::default().run(&scratch, "import('./entry.mjs');");
    assert_eq!(ran.exit, RealmExit::Exited(0), "{}", ran.stderr);
    assert_eq!(ran.stdout, "first\nTypeError\n");
    std::fs::remove_dir_all(&scratch.0).unwrap();
}

#[test]
fn require_resolves_exact_scoped_exports_before_literal_subpaths() {
    let scratch = Scratch::new("require-scoped-exports");
    std::fs::create_dir_all(scratch.0.join("node_modules/@scope/safe/dist")).unwrap();
    scratch.file("node_modules/@scope/safe/package.json", r#"{"type":"module","exports":{"./temp":{"types":"./dist/temp.d.ts","default":"./dist/temp.js"},"./ordered":{"node":{"import":"./wrong.js"},"default":[null,{"require":"./dist/temp.js"}]},"./blocked":null}}"#);
    scratch.file(
        "node_modules/@scope/safe/dist/temp.js",
        "export const value = 'actual';",
    );
    scratch.file(
        "node_modules/@scope/safe/blocked.js",
        "throw new Error('blocked subpath bypassed exports');",
    );
    let ran = Realm::default().run(&scratch, "console.log(require('@scope/safe/temp').value, require('@scope/safe/ordered').value); try { require('@scope/safe/blocked'); } catch (error) { console.log(error.code); }");
    assert_eq!(ran.exit, RealmExit::Exited(0), "{}", ran.stderr);
    assert_eq!(ran.stdout, "actual actual\nERR_PACKAGE_PATH_NOT_EXPORTED\n");
    std::fs::remove_dir_all(&scratch.0).unwrap();
}

#[test]
fn esm_and_require_map_scoped_export_patterns_to_their_own_conditions() {
    let scratch = Scratch::new("scoped-export-patterns");
    std::fs::create_dir_all(scratch.0.join("node_modules/@scope/sdk/dist")).unwrap();
    scratch.file("node_modules/@scope/sdk/package.json", r#"{"type":"module","exports":{"./*":{"import":"./dist/esm-*.js","require":"./dist/cjs-*.cjs"},"./blocked":null}}"#);
    scratch.file(
        "node_modules/@scope/sdk/dist/esm-types.js",
        "export const value = 'esm';",
    );
    scratch.file(
        "node_modules/@scope/sdk/dist/cjs-types.cjs",
        "exports.value = 'cjs';",
    );
    scratch.file(
        "entry.mjs",
        "import {value} from '@scope/sdk/types'; console.log(value);",
    );
    let ran = Realm::default().run(&scratch, "console.log(require('@scope/sdk/types').value); try { require('@scope/sdk/blocked'); } catch (error) { console.log(error.code); } import('./entry.mjs');");
    assert_eq!(ran.exit, RealmExit::Exited(0), "{}", ran.stderr);
    assert_eq!(ran.stdout, "cjs\nERR_PACKAGE_PATH_NOT_EXPORTED\nesm\n");
    std::fs::remove_dir_all(&scratch.0).unwrap();
}

#[test]
fn node_sqlite_uses_real_storage_binding_and_transaction_state() {
    let scratch = Scratch::new("node-sqlite");
    let ran = Realm::default().run(&scratch, r#"
        const { DatabaseSync } = require('node:sqlite');
        const db = new DatabaseSync('state.sqlite');
        console.log(db.location() === require('path').resolve('state.sqlite'));
        db.exec("ATTACH DATABASE ':memory:' AS mem"); console.log(db.location('mem'));
        db.prepare('ATTACH DATABASE ? AS disk').run(require('path').resolve('attached.sqlite'));
        console.log(db.location('disk') === require('path').resolve('attached.sqlite'));
        try { db.prepare('SELECT "unsupported string"').get(); } catch(error) { console.log(error.code); }
        db.exec('CREATE TABLE values_test(id INTEGER PRIMARY KEY, value TEXT)');
        const columns = db.prepare('SELECT value AS label, 42 AS calculated FROM values_test').columns();
        console.log(columns.map(column => column.name).join(','), columns[0].type, columns[1].type);
        console.log(db.isOpen, db.isTransaction);
        db.exec('BEGIN'); console.log(db.isTransaction);
        const insert = db.prepare('INSERT INTO values_test(value) VALUES($value)');
        console.log(insert.run({value:'retained'}).changes);
        db.exec('COMMIT'); console.log(db.isTransaction);
        const large = db.prepare('SELECT 9007199254740993 AS value');
        large.setReadBigInts(true); console.log(String(large.get().value));
        db.exec('BEGIN; INSERT INTO values_test(value) VALUES(\'rolled back\'); ROLLBACK');
        db.close(); console.log(db.isOpen);
        const reopened = new DatabaseSync('state.sqlite', {readOnly:true});
        console.log(reopened.prepare('SELECT value FROM values_test').get().value);
        try { reopened.exec('DELETE FROM values_test'); } catch(error) { console.log(error.code); }
        reopened.close();
        const quoted = new DatabaseSync(':memory:', {enableDoubleQuotedStringLiterals:true});
        console.log(quoted.prepare('SELECT "enabled string" AS value').get().value);
        quoted.close();
        let extensible;
        try { extensible = new DatabaseSync(':memory:', {allowExtension:true}); } catch(error) { if (!/does not support/.test(error.message)) throw error; }
        if (extensible) {
          try { extensible.prepare("SELECT load_extension('/does/not/exist')").get(); console.log('sql loaded'); } catch(error) { console.log('sql refused'); }
          try { extensible.loadExtension('/does/not/exist'); console.log('loaded'); } catch(error) { console.log(error.code !== 'ERR_INVALID_STATE'); }
          extensible.close();
        } else { console.log('sql refused'); console.log(true); }
    "#);
    assert_eq!(ran.exit, RealmExit::Exited(0), "{}", ran.stderr);
    assert_eq!(ran.stdout, "true\nnull\ntrue\nERR_SQLITE_ERROR\nlabel,calculated TEXT null\ntrue false\ntrue\n1\nfalse\n9007199254740993\nfalse\nretained\nERR_SQLITE_ERROR\nenabled string\nsql refused\ntrue\n");
    std::fs::remove_dir_all(&scratch.0).unwrap();
}

#[test]
fn text_decoder_windows_1252_matches_node_all_bytes_views_and_streams() {
    let scratch = Scratch::new("windows1252");
    let ran = Realm::default().run(&scratch, r#"
        const assert = require('node:assert/strict');
        const { createHash } = require('node:crypto');
        const all = Uint8Array.from({ length: 256 }, (_, i) => i);
        // Node 26 oracle: all 256 bytes decoded in fatal mode, then hashed as UTF-8.
        const decoded = new TextDecoder('latin1', { fatal: true }).decode(all);
        assert.equal(createHash('sha256').update(decoded).digest('hex'), 'cc916e51644a12e8de4ad160910c171a58621ee5dc3a6da6f8b00f8684085f33');
        for (const label of ['ascii', 'cp1252', 'cp819', 'csisolatin1', 'ibm819', 'iso-8859-1', 'iso-ir-100', 'iso88591', 'l1', 'us-ascii', 'x-cp1252', ' \tWINDOWS-1252\r\n']) {
            assert.equal(new TextDecoder(label).encoding, 'windows-1252');
        }
        const bytes = Uint8Array.from([0x41, 0x80, 0x81, 0x9f, 0x42]);
        const decoder = new TextDecoder('latin1', { fatal: true });
        assert.deepEqual([...decoder.decode(new DataView(bytes.buffer, 1, 3))].map(c => c.codePointAt(0)), [0x20ac, 0x81, 0x178]);
        assert.equal(decoder.decode(bytes.subarray(1, 2), { stream: true }), '\u20ac');
        assert.equal(decoder.decode(), '');
        assert.equal(decoder.decode(Uint8Array.of(0xff)), '\u00ff');
        for (const ignoreBOM of [false, true]) {
            assert.deepEqual([...new TextDecoder('latin1', { ignoreBOM }).decode(Uint8Array.of(0xef, 0xbb, 0xbf))].map(c => c.charCodeAt(0)), [0xef, 0xbb, 0xbf]);
        }
        assert.throws(() => decoder.decode([]), TypeError);
        assert.throws(() => new TextDecoder('\u00a0latin1'), RangeError);
        assert.throws(() => new TextDecoder('shift-jis'), RangeError);
        const utf8 = new TextDecoder(' unicode20utf8 ', { fatal: true });
        assert.equal(utf8.decode(Uint8Array.of(0xe2, 0x82), { stream: true }), '');
        assert.equal(utf8.decode(Uint8Array.of(0xac)), '\u20ac');
        console.log('whatwg windows1252 passed');
    "#);
    assert_eq!(ran.exit, RealmExit::Exited(0), "{}", ran.stderr);
    assert_eq!(ran.stdout, "whatwg windows1252 passed\n");
}

#[test]
fn fs_glob_sync_reads_real_scoped_files_and_exclusions() {
    let scratch = Scratch::new("fs-glob");
    std::fs::create_dir_all(scratch.0.join("src/nested")).unwrap();
    std::fs::create_dir_all(scratch.0.join("src/.hidden/.deep")).unwrap();
    scratch.file("src/main.ts", "main");
    scratch.file("src/nested/lib.rs", "lib");
    scratch.file("src/nested/notes.txt", "notes");
    scratch.file("src/.secret.ts", "secret");
    scratch.file("src/.hidden/inside.ts", "inside");
    scratch.file("src/.hidden/.secret.ts", "hidden file");
    scratch.file("src/.hidden/.deep/deep.ts", "hidden directory");
    let ran = Realm::default().run(&scratch, r#"
        const assert = require('node:assert/strict');
        const fs = require('node:fs');
        const path = require('node:path');
        const cwd = path.resolve('src');
        assert.deepEqual(fs.globSync('**/*.{ts,rs}', { cwd }).sort(), ['main.ts', path.join('nested', 'lib.rs')].sort());
        assert.deepEqual(fs.globSync(['*.ts', '**/*.ts'], { cwd }), ['main.ts']);
        assert.deepEqual(fs.globSync('nested/*', { cwd, exclude: ['**/*.txt'] }), [path.join('nested', 'lib.rs')]);
        assert.deepEqual(fs.globSync('**/*.rs', { cwd, exclude: p => p.includes('nested') }), []);
        assert.deepEqual(fs.globSync('nested', { cwd }), ['nested']);
        assert.deepEqual(fs.globSync('nested/**', { cwd }).sort(), ['nested', path.join('nested', 'lib.rs'), path.join('nested', 'notes.txt')].sort());
        assert.deepEqual(fs.globSync('**', { cwd }).sort(), ['.', 'main.ts', 'nested', path.join('nested', 'lib.rs'), path.join('nested', 'notes.txt')].sort());
        let excludedType;
        fs.globSync('**/*.rs', { cwd, withFileTypes: true, exclude: entry => { excludedType = entry instanceof fs.Dirent; return false; } });
        assert.equal(excludedType, true);
        assert.throws(() => fs.globSync('*', { cwd, exclude: () => true }), /not supported/);
        assert.deepEqual(fs.globSync('.hidden/*.ts', { cwd }), [path.join('.hidden', 'inside.ts')]);
        assert.deepEqual(fs.globSync('.hidden/**/*.ts', { cwd }), [path.join('.hidden', 'inside.ts')]);
        const [entry] = fs.globSync('nested/*.rs', { cwd, withFileTypes: true });
        assert.equal(entry.name, 'lib.rs');
        assert.equal(entry.isFile(), true);
        assert.equal(entry.parentPath, path.join(cwd, 'nested'));
        assert.deepEqual(fs.globSync(path.join(cwd, '*.ts')), [path.join(cwd, 'main.ts')]);
        assert.throws(() => fs.globSync('@(a|b)'), /not supported/);
        assert.throws(() => fs.globSync('*', { followSymlinks: true }), /not supported/);
        assert.equal(path.matchesGlob('.secret.ts', '*.ts'), false);
        assert.equal(path.matchesGlob('main.ts', '*.{ts,rs}'), true);
        if (process.platform !== 'win32') {
            fs.symlinkSync('nested', path.join(cwd, 'link'), 'dir');
            fs.symlinkSync('..', path.join(cwd, 'nested', 'loop'), 'dir');
            assert.deepEqual(fs.globSync('**/*.rs', { cwd }), [path.join('nested', 'lib.rs')]);
            assert.deepEqual(fs.globSync('link/*.rs', { cwd }), [path.join('link', 'lib.rs')]);
            const [link] = fs.globSync('link/**', { cwd, withFileTypes: true });
            assert.equal(link.isSymbolicLink(), true);
        }
        console.log('real fs glob passed');
    "#);
    assert_eq!(ran.exit, RealmExit::Exited(0), "{}", ran.stderr);
    assert_eq!(ran.stdout, "real fs glob passed\n");
}

#[test]
fn require_load_hooks_preserve_esm_source_and_filename_policy() {
    let scratch = Scratch::new("hook-esm");
    std::fs::create_dir_all(scratch.0.join("esm")).unwrap();
    scratch.file("esm/package.json", r#"{"type":"module"}"#);
    scratch.file("esm/index.js", "export const value = 'original';");
    scratch.file("esm/dep.mjs", "export const dep = 'dependency';");
    scratch.file("esm/kept.cjs", "module.exports = 'kept-commonjs';");
    scratch.file("untyped.js", "export const value = 'detected';");
    scratch.file(
        "explicit.cjs",
        "throw new Error('original must not execute');",
    );
    let ran = Realm::default().run(&scratch, r#"
        const assert = require('node:assert/strict');
        const { registerHooks } = require('node:module');
        const hooks = registerHooks({ load(url, context, next) {
            const loaded = next(url, context);
            if (url.endsWith('/esm/index.js')) return { ...loaded,
                source: "import { dep } from './dep.mjs'; export const value = dep + '-transformed';" };
            if (url.endsWith('/explicit.cjs')) return { ...loaded, format: 'module',
                source: "export const value = 'explicit-module';" };
            return loaded;
        }});
        assert.equal(require('./esm/index.js').value, 'dependency-transformed');
        assert.equal(require('./esm/kept.cjs'), 'kept-commonjs');
        assert.equal(require('./untyped.js').value, 'detected');
        assert.equal(require('./explicit.cjs').value, 'explicit-module');
        hooks.deregister();
        console.log('hook esm source passed');
    "#);
    assert_eq!(ran.exit, RealmExit::Exited(0), "{}", ran.stderr);
    assert_eq!(ran.stdout, "hook esm source passed\n");
}

#[test]
fn require_resolve_paths_reports_real_local_lookup_directories() {
    let scratch = Scratch::new("require-lookup-paths");
    std::fs::create_dir_all(scratch.0.join("nested/node_modules/pkg")).unwrap();
    scratch.file(
        "nested/node_modules/pkg/index.js",
        "module.exports = 'resolved';",
    );
    let ran = Realm::default().run(
        &scratch,
        r#"
        const path = require('node:path'), Module = require('node:module');
        const base = path.resolve('nested');
        const scoped = Module.createRequire(path.join(base, 'entry.cjs'));
        console.log(scoped.resolve.paths('node:fs'), scoped.resolve.paths('fs'));
        console.log(scoped.resolve.paths('./relative')[0] === base);
        const lookup = scoped.resolve.paths('pkg');
        const local = Module._nodeModulePaths(base);
        console.log(JSON.stringify(lookup.slice(0, local.length)) === JSON.stringify(local));
        console.log(scoped.resolve('pkg') === path.join(lookup[0], 'pkg/index.js'), scoped('pkg'));
        console.log(scoped.resolve.paths('/absolute')[0] === lookup[0]);
        try { scoped.resolve.paths(42); } catch(error) { console.log(error.name); }
    "#,
    );
    assert_eq!(ran.exit, RealmExit::Exited(0), "{}", ran.stderr);
    assert_eq!(
        ran.stdout,
        "null null\ntrue\ntrue\ntrue resolved\ntrue\nTypeError\n"
    );
    std::fs::remove_dir_all(&scratch.0).unwrap();
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
    assert_eq!(
        ran.stderr, "",
        "the termination is not reported as an error"
    );
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
    assert_eq!(
        ran.stdout,
        "helped bundle.cjs true
"
    );
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
    let printed = ran
        .stdout
        .trim()
        .strip_prefix("0 ")
        .unwrap_or("")
        .to_string();
    assert_eq!(
        Path::new(&printed).canonicalize().ok(),
        Some(scratch.0.clone()),
        "stdout: {}",
        ran.stdout
    );
}

#[test]
fn node_sqlite_disables_native_extension_loading_and_enforces_constructor_policy() {
    let mut runtime = lumen_runtime::Runtime::new();
    let source = r#"
 const assert=require('node:assert/strict');const {DatabaseSync}=require('node:sqlite');
 const options={allowExtension:false};const db=new DatabaseSync(':memory:',options);
 options.allowExtension=true;
 assert.equal(db.enableLoadExtension(false),undefined);
 assert.throws(()=>db.enableLoadExtension(true),{code:'ERR_INVALID_STATE'});
 assert.throws(()=>db.enableLoadExtension(0),{code:'ERR_INVALID_ARG_TYPE'});
 assert.throws(()=>db.loadExtension('/does/not/exist'),{code:'ERR_INVALID_STATE'});
 assert.throws(()=>db.prepare("SELECT load_extension('/does/not/exist')").get(),/not authorized|no such function/);
 db.close();assert.throws(()=>db.enableLoadExtension(false),/not open/);
 let ext;
 try { ext=new DatabaseSync(':memory:',{allowExtension:true}); } catch(error) { assert.match(error.message,/does not support/); }
 if(ext) {
   assert.throws(()=>ext.prepare("SELECT load_extension('/does/not/exist')").get(),/not authorized|no such function/);
   assert.throws(()=>ext.loadExtension('/does/not/exist'),(error)=>error.code!=='ERR_INVALID_STATE');
   ext.enableLoadExtension(false);
   assert.throws(()=>ext.loadExtension('/does/not/exist'),{code:'ERR_INVALID_STATE'});
   ext.enableLoadExtension(true);
   assert.throws(()=>ext.prepare("SELECT load_extension('/does/not/exist')").get(),/not authorized|no such function/);
   ext.close();
 }
 "#;
    match runtime.eval(source).expect("source parses") {
        lumen_runtime::Completion::Value(_) => {}
        lumen_runtime::Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }
}

#[test]
fn node_worker_uses_package_module_type_for_js_entry_with_top_level_await() {
    let scratch = Scratch::new("worker-package-module");
    scratch.file("package.json", r#"{"type":"module"}"#);
    scratch.file("worker.js", "import {parentPort} from 'node:worker_threads'; const answer = await new Promise(resolve=>setTimeout(()=>resolve(42), 10)); parentPort.postMessage(answer); parentPort.close();");
    let ran = Realm::default().run(&scratch, "const {Worker}=require('node:worker_threads'); const worker=new Worker('./worker.js'); worker.on('message', value=>console.log(value)); worker.on('error', error=>{console.error(error.message);process.exitCode=1;});");
    assert_eq!(ran.exit, RealmExit::Exited(0), "{}", ran.stderr);
    assert_eq!(ran.stdout, "42\n");
    std::fs::remove_dir_all(&scratch.0).unwrap();
}

#[test]
fn share_env_exchanges_live_writes_deletions_and_descriptors_without_host_mutation() {
    let scratch = Scratch::new("share-env-live");
    let key = "LUMEN_EMBEDDING_SHARE_ENV_TEST";
    let host_before = std::env::var_os(key);
    let mut realm = Realm::default();
    realm.env = vec![(key.into(), "initial".into()), ("DELETE_ME".into(), "old".into())];
    let ran = realm.run(&scratch, r#"
        const {Worker,SHARE_ENV}=require('node:worker_threads');
        const kept=process.env;
        const worker=new Worker(`
            const {parentPort}=require('node:worker_threads');
            parentPort.on('message',()=>{
                const read=process.env.LUMEN_EMBEDDING_SHARE_ENV_TEST;
                process.env.LUMEN_EMBEDDING_SHARE_ENV_TEST=42;
                delete process.env.DELETE_ME;
                process.env.FROM_WORKER='new';
                parentPort.postMessage(read);
                parentPort.close();
            });
        `,{eval:true,env:SHARE_ENV});
        worker.on('online',()=>{process.env.LUMEN_EMBEDDING_SHARE_ENV_TEST='updated';worker.postMessage('go');});
        worker.on('message',read=>console.log(JSON.stringify([
            read,kept.LUMEN_EMBEDDING_SHARE_ENV_TEST,typeof kept.LUMEN_EMBEDDING_SHARE_ENV_TEST,
            !('DELETE_ME' in kept),Object.keys(kept).includes('FROM_WORKER'),
            Object.getOwnPropertyDescriptor(kept,'FROM_WORKER').value
        ])));
        worker.on('error',e=>{console.error(e.stack);process.exitCode=1;});
    "#);
    assert_eq!(ran.exit, RealmExit::Exited(0), "{}", ran.stderr);
    assert_eq!(ran.stdout, "[\"updated\",\"42\",\"string\",true,true,\"new\"]\n");
    assert_eq!(std::env::var_os(key), host_before);
    std::fs::remove_dir_all(&scratch.0).unwrap();
}

#[test]
fn copied_worker_environment_remains_isolated_and_nested_sharing_stays_in_its_group() {
    let scratch = Scratch::new("share-env-nested");
    let mut realm = Realm::default();
    realm.env = vec![("GROUP".into(), "root".into())];
    let ran = realm.run(&scratch, r#"
        const {Worker}=require('node:worker_threads');
        const child=new Worker(`
            const {Worker,SHARE_ENV,parentPort}=require('node:worker_threads');
            process.env.GROUP='child';
            const nested=new Worker("process.env.GROUP='nested';",{eval:true,env:SHARE_ENV});
            nested.on('exit',()=>{parentPort.postMessage([process.env.GROUP,process.env.ONLY_ROOT===undefined]);parentPort.close();});
            nested.on('error',e=>{throw e;});
        `,{eval:true,env:{GROUP:'copy'}});
        process.env.ONLY_ROOT='root-only';
        child.on('message',value=>console.log(JSON.stringify([value,process.env.GROUP])));
        child.on('error',e=>{console.error(e.stack);process.exitCode=1;});
    "#);
    assert_eq!(ran.exit, RealmExit::Exited(0), "{}", ran.stderr);
    assert_eq!(ran.stdout, "[[\"nested\",true],\"root\"]\n");
    std::fs::remove_dir_all(&scratch.0).unwrap();
}

#[test]
fn fork_of_exec_path_runs_a_child_realm_with_ipc() {
    let scratch = Scratch::new("fork-ipc");
    scratch.file(
        "child.cjs",
        "process.on('message', (m) => {
           if (m === 'bye') { process.disconnect(); return; }
           process.send({ echo: m, argv: process.argv.slice(2), env: process.env.CHILD_MARK, cwd: process.cwd() });
         });
         process.send('ready');",
    );
    let ran = Realm::default().run(
        &scratch,
        "const { fork } = require('child_process');
         const child = fork(__dirname + '/child.cjs', ['a', 'b'], { env: { ...process.env, CHILD_MARK: 'x' } });
         const seen = [];
         child.on('message', (m) => {
           seen.push(m);
           if (m === 'ready') child.send({ n: 1, big: 10n === 10n });
           else child.send('bye');
         });
         child.on('exit', (code, signal) => console.log('exit', code, signal));
         child.on('close', () => console.log(JSON.stringify(seen), child.pid > 0, child.connected));",
    );
    assert_eq!(ran.exit, RealmExit::Exited(0), "{}", ran.stderr);
    let cwd = scratch.0.to_string_lossy().replace('\\', "\\\\");
    assert!(ran.stdout.contains("exit 0 null"), "{}", ran.stdout);
    assert!(
        ran.stdout.contains(&format!(
            r#"["ready",{{"echo":{{"n":1,"big":true}},"argv":["a","b"],"env":"x","cwd":"{cwd}"}}]"#
        )),
        "{}",
        ran.stdout
    );
    assert!(ran.stdout.contains("false"), "{}", ran.stdout);
    std::fs::remove_dir_all(&scratch.0).unwrap();
}

#[test]
fn fork_with_advanced_serialization_round_trips_structured_values() {
    let scratch = Scratch::new("fork-advanced");
    scratch.file(
        "child.cjs",
        "process.on('message', (m) => { process.send({ big: m.big * 2n, map: [...m.map], date: m.date instanceof Date }); process.exit(7); });",
    );
    let ran = Realm::default().run(
        &scratch,
        "const { fork } = require('child_process');
         const child = fork(__dirname + '/child.cjs', { serialization: 'advanced' });
         child.on('message', (m) => console.log(typeof m.big, String(m.big), JSON.stringify(m.map), m.date));
         child.on('exit', (code) => console.log('exit', code));
         child.send({ big: 21n, map: new Map([[1, 2]]), date: new Date(0) });",
    );
    assert_eq!(ran.exit, RealmExit::Exited(0), "{}", ran.stderr);
    assert!(ran.stdout.contains("bigint 42 [[1,2]] true"), "{}", ran.stdout);
    assert!(ran.stdout.contains("exit 7"), "{}", ran.stdout);
    std::fs::remove_dir_all(&scratch.0).unwrap();
}

#[test]
fn killing_a_child_realm_reports_the_signal() {
    let scratch = Scratch::new("child-kill");
    scratch.file(
        "child.cjs",
        "process.send('up'); setInterval(() => {}, 1000); for (;;) {}",
    );
    let ran = Realm::default().run(
        &scratch,
        "const { fork } = require('child_process');
         const child = fork(__dirname + '/child.cjs');
         child.on('message', () => console.log('killed', child.kill()));
         child.on('exit', (code, signal) => console.log('exit', code, signal, child.killed));",
    );
    assert_eq!(ran.exit, RealmExit::Exited(0), "{}", ran.stderr);
    assert!(ran.stdout.contains("killed true"), "{}", ran.stdout);
    assert!(ran.stdout.contains("exit null SIGTERM true"), "{}", ran.stdout);
    std::fs::remove_dir_all(&scratch.0).unwrap();
}

#[test]
fn spawn_of_exec_path_pipes_stdio_to_a_child_realm() {
    let scratch = Scratch::new("spawn-pipe");
    scratch.file(
        "child.cjs",
        "process.stdin.on('data', (d) => process.stdout.write('got:' + d));
         process.stdin.on('end', () => { console.error('child-err'); process.exit(3); });",
    );
    let ran = Realm::default().run(
        &scratch,
        "const { spawn } = require('child_process');
         const child = spawn(process.execPath, [__dirname + '/child.cjs'], { stdio: ['pipe', 'pipe', 'pipe'] });
         let out = '', err = '';
         child.stdout.on('data', (d) => out += d);
         child.stderr.on('data', (d) => err += d);
         child.on('close', (code, signal) => console.log(JSON.stringify({ out, err, code, signal })));
         child.stdin.write('one');
         setTimeout(() => child.stdin.end(), 50);",
    );
    assert_eq!(ran.exit, RealmExit::Exited(0), "{}", ran.stderr);
    assert!(
        ran.stdout
            .contains(r#"{"out":"got:one","err":"child-err\n","code":3,"signal":null}"#),
        "{}",
        ran.stdout
    );
    std::fs::remove_dir_all(&scratch.0).unwrap();
}

#[test]
fn child_realm_inherits_the_parent_streams() {
    let scratch = Scratch::new("spawn-inherit");
    scratch.file("child.cjs", "console.log('from child'); console.error('child err');");
    let ran = Realm::default().run(
        &scratch,
        "require('child_process').spawn(process.execPath, [__dirname + '/child.cjs'], { stdio: 'inherit' });",
    );
    assert_eq!(ran.exit, RealmExit::Exited(0), "{}", ran.stderr);
    assert_eq!(ran.stdout, "from child\n");
    assert!(ran.stderr.contains("child err"), "{}", ran.stderr);
    std::fs::remove_dir_all(&scratch.0).unwrap();
}

#[test]
fn ending_the_parent_realm_stops_its_children() {
    let scratch = Scratch::new("child-orphan");
    scratch.file("child.cjs", "setInterval(() => {}, 1000); for (;;) {}");
    let started = Instant::now();
    let ran = Realm::default().run(
        &scratch,
        "const child = require('child_process').spawn(process.execPath, [__dirname + '/child.cjs']);
         child.unref();
         setTimeout(() => process.exit(5), 200);",
    );
    assert_eq!(ran.exit, RealmExit::Exited(5), "{}", ran.stderr);
    assert!(started.elapsed() < Duration::from_secs(20));
    std::fs::remove_dir_all(&scratch.0).unwrap();
}

#[test]
fn a_nested_child_realm_starts_children_of_its_own() {
    let scratch = Scratch::new("child-nested");
    scratch.file("leaf.cjs", "process.send('leaf'); process.exit(0);");
    scratch.file(
        "mid.cjs",
        "const child = require('child_process').fork(__dirname + '/leaf.cjs');
         child.on('message', (m) => process.send('mid saw ' + m));",
    );
    let ran = Realm::default().run(
        &scratch,
        "const child = require('child_process').fork(__dirname + '/mid.cjs');
         child.on('message', (m) => console.log(m));",
    );
    assert_eq!(ran.exit, RealmExit::Exited(0), "{}", ran.stderr);
    assert_eq!(ran.stdout, "mid saw leaf\n");
    std::fs::remove_dir_all(&scratch.0).unwrap();
}

#[test]
fn spawn_of_exec_path_with_ignored_stdin_and_ipc_slot() {
    let scratch = Scratch::new("spawn-ipc-slot");
    scratch.file(
        "child.cjs",
        "process.on('message', (m) => { process.send({ id: m.id, result: { ok: true } }); });
         console.error('worker started');",
    );
    let ran = Realm::default().run(
        &scratch,
        "const { spawn } = require('child_process');
         const env = { A: '1' };
         const child = spawn(process.execPath, [__dirname + '/child.cjs'], { env, cwd: __dirname, stdio: ['ignore', 'pipe', 'pipe', 'ipc'] });
         let err = '';
         child.stderr.on('data', (d) => err += d);
         child.on('error', (e) => console.log('error', e.message));
         child.once('spawn', () => console.log('spawned'));
         child.on('message', (m) => { console.log(JSON.stringify(m)); child.kill('SIGKILL'); });
         child.on('close', (code, signal) => console.log('close', code, signal, JSON.stringify(err)));
         child.send({ id: 1 });",
    );
    assert_eq!(ran.exit, RealmExit::Exited(0), "{}", ran.stderr);
    assert_eq!(
        ran.stdout,
        "spawned\n{\"id\":1,\"result\":{\"ok\":true}}\nclose null SIGKILL \"worker started\\n\"\n"
    );
    std::fs::remove_dir_all(&scratch.0).unwrap();
}

#[test]
fn a_child_blocked_on_an_unread_pipe_does_not_outlive_its_parent() {
    let scratch = Scratch::new("child-full-pipe");
    scratch.file(
        "child.cjs",
        "const chunk = 'x'.repeat(65536); for (;;) process.stdout.write(chunk);",
    );
    let started = Instant::now();
    let ran = Realm::default().run(
        &scratch,
        "const child = require('child_process').spawn(process.execPath, [__dirname + '/child.cjs']);
         child.unref();
         setTimeout(() => process.exit(5), 400);",
    );
    assert_eq!(ran.exit, RealmExit::Exited(5), "{}", ran.stderr);
    assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
    std::fs::remove_dir_all(&scratch.0).unwrap();
}

#[test]
fn killing_a_child_blocked_on_an_unread_pipe_frees_it() {
    let scratch = Scratch::new("child-full-pipe-kill");
    scratch.file(
        "child.cjs",
        "const chunk = 'x'.repeat(65536); for (;;) process.stdout.write(chunk);",
    );
    let started = Instant::now();
    let ran = Realm::default().run(
        &scratch,
        "const child = require('child_process').spawn(process.execPath, [__dirname + '/child.cjs']);
         child.on('exit', (code, signal) => console.log('exit', code, signal));
         setTimeout(() => child.kill('SIGKILL'), 400);",
    );
    assert_eq!(ran.exit, RealmExit::Exited(0), "{}", ran.stderr);
    assert!(ran.stdout.contains("exit null SIGKILL"), "{}", ran.stdout);
    assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
    std::fs::remove_dir_all(&scratch.0).unwrap();
}

#[test]
fn only_the_ipc_slot_gets_the_channel_descriptor() {
    let scratch = Scratch::new("child-ipc-slots");
    scratch.file(
        "child.cjs",
        "process.on('message', (m) => { process.send({ echo: m, fd: process.env.NODE_CHANNEL_FD !== undefined }); process.exit(0); });",
    );
    let ran = Realm::default().run(
        &scratch,
        "const { spawn } = require('child_process');
         const child = spawn(process.execPath, [__dirname + '/child.cjs'], { stdio: ['pipe', 'pipe', 'pipe', 'ipc', 'pipe'] });
         child.on('message', (m) => console.log(JSON.stringify(m)));
         child.on('close', (code) => console.log('close', code));
         child.send('hi');",
    );
    assert_eq!(ran.exit, RealmExit::Exited(0), "{}", ran.stderr);
    assert!(ran.stdout.contains(r#"{"echo":"hi","fd":true}"#), "{}", ran.stdout);
    assert!(ran.stdout.contains("close 0"), "{}", ran.stdout);
    std::fs::remove_dir_all(&scratch.0).unwrap();
}

#[test]
fn a_signal_reaches_the_child_realms_listener() {
    let scratch = Scratch::new("child-signal-handler");
    scratch.file(
        "child.cjs",
        "process.on('SIGTERM', (name) => { process.send('handled ' + name); process.exit(3); });
         process.on('SIGWINCH', () => process.send('winch'));
         setInterval(() => {}, 1000); process.send('up');",
    );
    let ran = Realm::default().run(
        &scratch,
        "const { fork } = require('child_process');
         const child = fork(__dirname + '/child.cjs');
         child.on('message', (m) => {
           console.log(m);
           if (m === 'up') process.kill(child.pid, 'SIGWINCH');
           if (m === 'winch') child.kill('SIGTERM');
         });
         child.on('exit', (code, signal) => console.log('exit', code, signal));",
    );
    assert_eq!(ran.exit, RealmExit::Exited(0), "{}", ran.stderr);
    assert_eq!(ran.stdout, "up\nwinch\nhandled SIGTERM\nexit 3 null\n");
    std::fs::remove_dir_all(&scratch.0).unwrap();
}

#[test]
fn an_unhandled_catchable_signal_takes_its_default_action() {
    let scratch = Scratch::new("child-signal-default");
    scratch.file("child.cjs", "setInterval(() => {}, 1000); process.send('up');");
    let ran = Realm::default().run(
        &scratch,
        "const { fork } = require('child_process');
         const child = fork(__dirname + '/child.cjs');
         child.on('message', () => { child.kill('SIGWINCH'); setTimeout(() => { console.log('alive', child.exitCode); child.kill('SIGINT'); }, 100); });
         child.on('exit', (code, signal) => console.log('exit', code, signal));",
    );
    assert_eq!(ran.exit, RealmExit::Exited(0), "{}", ran.stderr);
    assert_eq!(ran.stdout, "alive null\nexit null SIGINT\n");
    std::fs::remove_dir_all(&scratch.0).unwrap();
}

#[test]
fn a_realm_tree_has_a_limit_on_live_children() {
    let scratch = Scratch::new("child-limit");
    scratch.file("child.cjs", "setInterval(() => {}, 1000);");
    scratch.file("quick.cjs", "process.exit(0);");
    let ran = Realm::default().run(
        &scratch,
        "const { spawn } = require('child_process');
         const live = [];
         let refused = null;
         for (let i = 0; i < 9; i++) {
           const child = spawn(process.execPath, [__dirname + '/child.cjs']);
           child.on('error', (e) => { refused = e.code; });
           live.push(child);
         }
         setTimeout(() => {
           console.log('refused', refused);
           for (const child of live) child.kill('SIGKILL');
           setTimeout(() => {
             const again = spawn(process.execPath, [__dirname + '/quick.cjs']);
             again.on('error', (e) => console.log('again failed', e.code));
             again.on('exit', (code) => console.log('again', code));
           }, 300);
         }, 300);",
    );
    assert_eq!(ran.exit, RealmExit::Exited(0), "{}", ran.stderr);
    assert!(ran.stdout.contains("refused EAGAIN"), "{}", ran.stdout);
    assert!(ran.stdout.contains("again 0"), "{}", ran.stdout);
    std::fs::remove_dir_all(&scratch.0).unwrap();
}

#[test]
fn a_child_realm_over_the_heap_ceiling_reports_an_abort() {
    let scratch = Scratch::new("child-heap");
    scratch.file("child.cjs", "const keep = []; for (;;) keep.push({});");
    let ran = Realm {
        live_object_limit: Some(200_000),
        ..Realm::default()
    }
    .run(
        &scratch,
        "const child = require('child_process').spawn(process.execPath, [__dirname + '/child.cjs']);
         child.on('exit', (code, signal) => console.log('exit', code, signal));",
    );
    assert_eq!(ran.exit, RealmExit::Exited(0), "{}", ran.stderr);
    assert!(ran.stdout.contains("exit null SIGABRT"), "{}", ran.stdout);
    std::fs::remove_dir_all(&scratch.0).unwrap();
}

#[test]
fn spawn_sync_of_exec_path_runs_a_child_realm() {
    let scratch = Scratch::new("spawn-sync");
    scratch.file(
        "child.cjs",
        "let input = ''; process.stdin.on('data', (d) => input += d);
         process.stdin.on('end', () => { console.log('out:' + input + ':' + process.argv.slice(2)); console.error('err'); process.exit(4); });",
    );
    scratch.file("loud.cjs", "for (;;) process.stdout.write('x'.repeat(4096));");
    scratch.file("slow.cjs", "setInterval(() => {}, 1000);");
    let ran = Realm::default().run(
        &scratch,
        "const { spawnSync, execFileSync } = require('child_process');
         const done = spawnSync(process.execPath, [__dirname + '/child.cjs', 'a'], { input: 'in', encoding: 'utf8' });
         console.log(JSON.stringify([done.status, done.signal, done.stdout, done.stderr, done.error === undefined]));
         const loud = spawnSync(process.execPath, [__dirname + '/loud.cjs'], { maxBuffer: 10000 });
         console.log(loud.error && loud.error.code, loud.signal);
         const slow = spawnSync(process.execPath, [__dirname + '/slow.cjs'], { timeout: 200 });
         console.log(slow.error && slow.error.code, slow.signal);
         try { execFileSync(process.execPath, ['-r', 'x', '--', __dirname + '/child.cjs'], { stdio: ['pipe', 'pipe', 'ignore'], input: '' }); } catch (e) { console.log('threw', e.status); }",
    );
    assert_eq!(ran.exit, RealmExit::Exited(0), "{}", ran.stderr);
    assert!(ran.stdout.contains("threw 4"), "{}", ran.stdout);
    assert!(ran.stdout.contains(r#"[4,null,"out:in:a\n","err\n",true]"#), "{}", ran.stdout);
    assert!(ran.stdout.contains("ENOBUFS SIGTERM"), "{}", ran.stdout);
    assert!(ran.stdout.contains("ETIMEDOUT SIGTERM"), "{}", ran.stdout);
    std::fs::remove_dir_all(&scratch.0).unwrap();
}
