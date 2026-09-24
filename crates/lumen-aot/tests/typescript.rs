//! TypeScript sources in an AOT bundle: `.mts`/`.ts`/`.cts` files are type-stripped (Node's
//! strip-only erasure, offsets kept) as the graph is walked, then run from the blob.

use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

use lumen_runtime::{ConsoleOut, Runtime};

#[derive(Clone, Default)]
struct Captured(Rc<RefCell<Vec<u8>>>);
impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn bundles_and_runs_typescript() {
    let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let spec = lumen_aot::build::Spec {
        entry: Some(base.join("tests/ts/main.mts")),
        walk: true,
        keep_source: vec!["**".into()],
        ..Default::default()
    };
    let out = std::env::temp_dir().join(format!("lumen-aot-ts-{}.aot", std::process::id()));
    lumen_aot::build::precompile_spec(&out, &spec).expect("bundle");
    let bytes: &'static [u8] = Box::leak(std::fs::read(&out).unwrap().into_boxed_slice());
    let _ = std::fs::remove_file(&out);
    let blob = lumen::Precompiled::from_static(bytes);

    let mut runtime = Runtime::new();
    let captured = Captured::default();
    runtime.engine().ctx().op_state().put(ConsoleOut {
        out: Box::new(captured.clone()),
        err: Box::new(Captured::default()),
    });
    runtime.run_precompiled(&blob).expect("run");
    // Node prints `9:18:126` (126 = the length of `total`'s source text, types blanked).
    assert_eq!(String::from_utf8(captured.0.borrow().clone()).unwrap(), "9:18:126\n");
}

#[test]
fn rejects_unsupported_typescript() {
    let dir = std::env::temp_dir().join(format!("lumen-aot-ts-bad-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("main.ts"), "export enum E { A }\n").unwrap();
    let spec = lumen_aot::build::Spec {
        entry: Some(dir.join("main.ts")),
        walk: true,
        ..Default::default()
    };
    let err = lumen_aot::build::precompile_spec(dir.join("out.aot"), &spec).unwrap_err();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        err.contains("ERR_UNSUPPORTED_TYPESCRIPT_SYNTAX") && err.contains("main.ts:1:8"),
        "{err}"
    );
}
