//! `lumen-node/src/js/esm_exports.js` lists each builtin's ESM named exports so startup does not
//! have to load every builtin to enumerate its keys. This keeps that list in sync with the
//! modules themselves: a builtin that gains or loses an export fails here until the list is
//! regenerated.

use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

use lumen_runtime::{Completion, ConsoleOut, Runtime};

#[derive(Clone, Default)]
struct Captured(Rc<RefCell<Vec<u8>>>);
impl Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn esm_export_lists_match_builtin_keys() {
    let mut runtime = Runtime::new();
    let out = Captured::default();
    runtime.engine().ctx().op_state().put(ConsoleOut {
        out: Box::new(out.clone()),
        err: Box::new(Captured::default()),
    });
    let source = r#"
      const ident = (k) => /^[A-Za-z_$][A-Za-z0-9_$]*$/.test(k) && k !== "default";
      const names = globalThis.__builtinNames.split(",").filter((n) => n !== "process");
      let checked = 0;
      for (const name of names) {
        const m = require("node:" + name);
        const actual = (m && (typeof m === "object" || typeof m === "function") ? Object.keys(m) : [])
          .filter(ident).sort();
        const src = globalThis.__esmBuiltinSources["node:" + name];
        if (src === undefined) { console.log("unlisted", name); continue; }
        const expected = [...src.matchAll(/^export const ([^ ]+) =/gm)].map((x) => x[1]).sort();
        const missing = actual.filter((k) => !expected.includes(k));
        const extra = expected.filter((k) => !actual.includes(k));
        if (missing.length || extra.length)
          console.log("mismatch", name, "missing:", missing.join(" "), "extra:", extra.join(" "));
        checked++;
      }
      console.log("checked", checked);
    "#;
    match runtime.eval(source).expect("source parses") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }
    let text = String::from_utf8(out.0.borrow().clone()).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 1, "esm_exports.js is out of date:\n{text}");
    assert!(lines[0].starts_with("checked "), "{text}");
}
