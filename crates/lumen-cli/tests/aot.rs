use std::path::Path;
use std::process::{Command, Output};

fn cli(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_lumen-cli"))
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap()
}

fn success(out: Output) -> String {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn compiles_and_runs_bundled_typescript_without_source_files() {
    let dir = std::env::temp_dir().join(format!("lumen-aot-cli-{}", std::process::id()));
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("value.mts"), "export const value: number = 41;").unwrap();
    std::fs::write(
        dir.join("app.mts"),
        "import {value} from './value.mts'; console.log(value + 1);",
    )
    .unwrap();
    success(cli(&dir, &["target", "device.target"]));
    success(cli(
        &dir,
        &[
            "compile",
            "app.mts",
            "--tier",
            "bc",
            "--target",
            "device.target",
        ],
    ));
    let first = std::fs::read(dir.join("app.lbc")).unwrap();
    success(cli(
        &dir,
        &["compile", "app.mts", "--tier", "bc", "-o", "second.lbc"],
    ));
    assert_eq!(
        first,
        std::fs::read(dir.join("second.lbc")).unwrap(),
        "non-deterministic blob"
    );
    assert!(!cli(&dir, &["compile", "app.mts", "-o", "app.mts"])
        .status
        .success());
    assert!(!cli(&dir, &["compile", "app.mts", "-o", "value.mts"])
        .status
        .success());
    assert!(!cli(&dir, &["compile", "app.mts", "--tier", "mc"])
        .status
        .success());
    std::fs::remove_file(dir.join("value.mts")).unwrap();
    std::fs::remove_file(dir.join("app.mts")).unwrap();
    assert_eq!(success(cli(&dir, &["run", "app.lbc"])).trim(), "42");
    assert_eq!(success(cli(&dir, &["app.lbc"])).trim(), "42");
    std::fs::write(dir.join("bad.lbc"), &first[..first.len() - 1]).unwrap();
    assert!(!cli(&dir, &["run", "bad.lbc"]).status.success());
    for name in ["app.lbc", "second.lbc", "device.target", "bad.lbc"] {
        std::fs::remove_file(dir.join(name)).unwrap();
    }
    std::fs::remove_dir(dir).unwrap();
}
