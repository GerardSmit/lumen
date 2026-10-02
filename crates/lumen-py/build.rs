use std::fmt::Write as _;
use std::path::{Path, PathBuf};

fn collect(dir: &Path, rel: &str, out: &mut Vec<(String, PathBuf)>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir).expect("read lib dir").filter_map(Result::ok).collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().to_string_lossy().into_owned();
        let path = e.path();
        let child = if rel.is_empty() { name.clone() } else { format!("{}/{}", rel, name) };
        if path.is_dir() {
            collect(&path, &child, out);
        } else if name.ends_with(".py") {
            out.push((child, path));
        }
    }
}

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let lib = manifest.join("lib");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", lib.display());
    let mut files = Vec::new();
    collect(&lib, "", &mut files);
    let mut src = String::from("pub static TABLE: &[(&str, &str)] = &[\n");
    for (rel, path) in &files {
        println!("cargo:rerun-if-changed={}", path.display());
        writeln!(src, "    ({:?}, include_str!({:?})),", rel, path.display().to_string()).unwrap();
    }
    src.push_str("];\n");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("frozen_table.rs");
    std::fs::write(out, src).expect("write frozen table");
}
