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

#[cfg(any(windows, target_os = "linux"))]
#[test]
fn standalone_runs_without_sources_and_owns_its_arguments() {
    let dir = std::env::temp_dir().join(format!("lumen-standalone-cli-{}", std::process::id()));
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("app.js"), "console.log(42); console.log(process.argv[2]);").unwrap();
    let name = if cfg!(windows) { "app.exe" } else { "app" };
    success(cli(&dir, &["compile", "app.js", "--script", "--exe", "-o", name]));
    std::fs::remove_file(dir.join("app.js")).unwrap();
    let output = Command::new(dir.join(name)).current_dir(&dir).arg("compile").output().unwrap();
    assert_eq!(success(output).replace('\r', ""), "42\ncompile\n");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn compiles_and_runs_bundled_typescript_without_source_files() {
    let dir = std::env::temp_dir().join(format!("lumen-aot-cli-{}", std::process::id()));
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("value.mts"), "export const value: number = 41;").unwrap();
    std::fs::write(
        dir.join("app.mts"),
        "import {value} from './value.mts'; class Box { constructor(v) { this.value = v; } } console.log(new Box(value + 1).value);",
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
    success(cli(&dir, &[
        "compile", "app.mts", "--profile", "nojit", "--compression", "false",
        "-o", "uncompressed.lbc",
    ]));
    assert_eq!(success(cli(&dir, &["run", "uncompressed.lbc"])).trim(), "42");
    assert!(!cli(&dir, &["compile", "app.mts", "-o", "app.mts"])
        .status
        .success());
    assert!(!cli(&dir, &["compile", "app.mts", "-o", "value.mts"])
        .status
        .success());
    success(cli(&dir, &["compile", "app.mts", "--tier", "mc"]));
    assert_eq!(success(cli(&dir, &["run", "app.lmc"])).trim(), "42");
    std::fs::remove_file(dir.join("value.mts")).unwrap();
    std::fs::remove_file(dir.join("app.mts")).unwrap();
    assert_eq!(success(cli(&dir, &["run", "app.lbc"])).trim(), "42");
    assert_eq!(success(cli(&dir, &["app.lbc"])).trim(), "42");
    std::fs::write(dir.join("bad.lbc"), &first[..first.len() - 1]).unwrap();
    assert!(!cli(&dir, &["run", "bad.lbc"]).status.success());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn runtime_import_union_deduplicates_and_rejects_abi_conflicts() {
    use lumen_common::aot::{self, native_data::{FunctionEntry, Import}, Section};
    let dir = std::env::temp_dir().join(format!("lumen-runtime-imports-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let write = |name: &str, signature| {
        let imports = [Import { module: "bitnest:net", name: "send", signature_hash: signature }];
        let data = aot::native_data::encode_with_imports(&[FunctionEntry { offset: 0, len: 4 }], &imports, b"fixture", 4).unwrap();
        let blob = aot::encode_native(aot::Language::JavaScript, 42, aot::version_bytes("0.1.0"), &[
            Section { kind: aot::SEC_NATIVE_CODE, flags: 0, data: b"code" },
            Section { kind: aot::SEC_NATIVE_DATA, flags: 0, data: &data },
            Section { kind: aot::SEC_NATIVE_GOT_RELOCS, flags: 0, data: &[] },
        ]).unwrap();
        std::fs::write(dir.join(name), blob).unwrap();
    };
    write("first.lmc", 1);
    write("second.lmc", 1);
    write("conflict.lmc", 2);
    let output = success(cli(&dir, &["runtime-imports", "first.lmc", "second.lmc"]));
    let json: serde_json::Value = serde_json::from_str(&output).unwrap();
    assert_eq!(json["imports"].as_array().unwrap().len(), 1);
    assert_eq!(json["imports"][0]["module"], "bitnest:net");
    assert_eq!(json["imports"][0]["signatureHash"], "0000000000000001");
    assert!(!cli(&dir, &["runtime-imports", "first.lmc", "conflict.lmc"]).status.success());
    for name in ["first.lmc", "second.lmc", "conflict.lmc"] { std::fs::remove_file(dir.join(name)).unwrap(); }
    std::fs::remove_dir(dir).unwrap();
}
