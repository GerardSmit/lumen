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
fn tty_streams_wrap_process_io_without_claiming_terminal_support() {
    let mut runtime = Runtime::new();
    let out = Captured::default();
    runtime.engine().ctx().op_state().put(ConsoleOut {
        out: Box::new(out.clone()),
        err: Box::new(Captured::default()),
    });
    // Whether fd 0/1 are consoles depends on how the test binary is launched (terminal, IDE,
    // piped CI), so the assertions only check shapes and invariants, never the detected values.
    // The WriteStream's sink is swapped for a capture so nothing reaches the real stdout.
    let source = r#"
      const tty = require("node:tty"), stream = require("node:stream");
      const input = new tty.ReadStream(0), output = new tty.WriteStream(1);
      console.log("shape", input instanceof stream.Readable, output instanceof stream.Writable, typeof tty.isatty(1), tty.isatty(-1), tty.isatty(1.5));
      console.log("input", input.fd, input.isTTY, input.setRawMode(true) === input, input.isRaw);
      const depth = output.getColorDepth(), size = output.getWindowSize();
      console.log("output", output.fd, output.isTTY, [1, 4, 8, 24].includes(depth), output.hasColors() === depth >= 4,
        output.getColorDepth({ FORCE_COLOR: "3" }), output.hasColors(256, { TERM: "dumb" }), Array.isArray(size) && size.length === 2);
      const written = [];
      output._write = (chunk, encoding, callback) => { written.push(String(chunk)); callback(); };
      console.log("cursor", output.clearLine(0), output.clearScreenDown(), output.cursorTo(0), output.moveCursor(1, 1));
      output.end("tty-write\n");
      console.log("written", JSON.stringify(written));
    "#;
    match runtime.eval(source).expect("source parses") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }
    assert_eq!(
        String::from_utf8(out.0.borrow().clone())
            .unwrap()
            .lines()
            .collect::<Vec<_>>(),
        [
            "shape true true boolean false false",
            "input 0 true true true",
            "output 1 true true true 24 false true",
            "cursor true true true true",
            r#"written ["\u001b[2K","\u001b[0J","\u001b[1G","\u001b[1C\u001b[1B","tty-write\n"]"#
        ]
    );
}
