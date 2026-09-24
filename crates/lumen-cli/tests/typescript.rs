use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_DIR: AtomicU64 = AtomicU64::new(1);

#[test]
fn runs_commonjs_typescript_entry_and_dependency() {
    let dir = std::env::temp_dir().join(format!(
        "lumen-typescript-test-{}-{}",
        std::process::id(),
        NEXT_DIR.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("value.ts"),
        "const value: number = 41; module.exports = value;\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("main.cts"),
        "const value: number = require('./value'); console.log(value + 1);\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_lumen-cli"))
        .arg(dir.join("main.cts"))
        .output()
        .expect("run TypeScript entry");
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), "42");
}

/// The TypeScript program corpus in `tests/ts`: each entry's stdout equals Node's (recorded in
/// `X.out` by `node tests/ts/gen.mjs`). The programs cover every erasable construct, ASI around
/// erased statements, non-ASCII in erased text (via `Function.prototype.toString`, which shows
/// the offsets hold), `.ts`/`.mts`/`.cts` module interop, and plain JavaScript that must stay
/// untouched.
#[test]
fn typescript_corpus_matches_node() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/ts");
    let mut entries: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            matches!(
                p.extension().and_then(|e| e.to_str()),
                Some("ts" | "mts" | "cts")
            )
        })
        .collect();
    entries.sort();
    assert!(entries.len() >= 8, "corpus missing in {}", dir.display());
    for entry in entries {
        let want = std::fs::read_to_string(entry.with_extension("out"))
            .unwrap_or_else(|e| panic!("{}: no .out ({e})", entry.display()))
            .replace("\r\n", "\n");
        let output = Command::new(env!("CARGO_BIN_EXE_lumen-cli"))
            .arg(&entry)
            .current_dir(&dir)
            .output()
            .expect("run lumen-cli");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{}: {stderr}", entry.display());
        let got = String::from_utf8(output.stdout)
            .unwrap()
            .replace("\r\n", "\n");
        assert_eq!(got, want, "{}: stdout differs from Node's", entry.display());
    }
}

/// `tests/ts/reject`: syntax that needs transformation (or does not parse) fails the program
/// with Node's error, whether it is the entry or an imported module.
#[test]
fn typescript_rejections_match_node() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/ts/reject");
    let mut entries: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            matches!(
                p.extension().and_then(|e| e.to_str()),
                Some("ts" | "mts" | "cts")
            )
        })
        .collect();
    entries.sort();
    assert!(entries.len() >= 6, "rejects missing in {}", dir.display());
    for entry in entries {
        let want = std::fs::read_to_string(entry.with_extension("err"))
            .unwrap_or_else(|e| panic!("{}: no .err ({e})", entry.display()));
        let (_code, message) = want.trim().split_once(' ').unwrap_or((want.trim(), ""));
        let output = Command::new(env!("CARGO_BIN_EXE_lumen-cli"))
            .arg(&entry)
            .current_dir(&dir)
            .output()
            .expect("run lumen-cli");
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(!output.status.success(), "{}: ran", entry.display());
        assert!(
            stderr.contains("SyntaxError"),
            "{}: {stderr}",
            entry.display()
        );
        assert!(
            stderr.contains(message),
            "{}: want {message:?}, got {stderr}",
            entry.display()
        );
        // Rejected before any of the file runs.
        assert!(!stdout.contains("before"), "{}: {stdout}", entry.display());
    }
}
