use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

mod support;

use lumen_runtime::{ConsoleOut, RealmExit, Runtime};

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
fn request_bodies_may_be_streams_that_produce_data_later() {
    support::on_engine_stack(request_bodies_may_be_streams_that_produce_data_later_body);
}

fn request_bodies_may_be_streams_that_produce_data_later_body() {
    let mut runtime = Runtime::new();
    let out = Captured::default();
    let err = Captured::default();
    runtime.engine().ctx().op_state().put(ConsoleOut {
        out: Box::new(out.clone()),
        err: Box::new(err.clone()),
    });
    let source = r#"
      const http = require("node:http");
      const slow = (parts, { fail } = {}) => {
        let i = 0;
        return new ReadableStream({
          async pull(controller) {
            await new Promise((resolve) => setTimeout(resolve, 5));
            if (fail && i === 1) return controller.error(new Error("source failed"));
            if (i < parts.length) controller.enqueue(new TextEncoder().encode(parts[i++]));
            else controller.close();
          },
        });
      };
      const server = http.createServer((req, res) => {
        const chunks = [];
        req.on("data", (c) => chunks.push(c));
        req.on("end", () => res.end(Buffer.concat(chunks)));
      });
      server.listen(0, "127.0.0.1", async () => {
        const url = `http://127.0.0.1:${server.address().port}/`;
        try {
          const res = await fetch(url, { method: "POST", body: slow(["a", "bc", "def"]), duplex: "half" });
          console.log("fetch", await res.text());

          try {
            new Request(url, { method: "POST", body: slow(["x"]) });
          } catch (e) {
            console.log("duplex", e.constructor.name, e.message);
          }

          const request = new Request(url, { method: "POST", body: slow(["one", "two"]), duplex: "half" });
          const copy = request.clone();
          console.log("request", await request.text(), await copy.text());

          const response = new Response(slow(["he", "llo"]));
          const twin = response.clone();
          console.log("response", await response.text(), await twin.text());

          await fetch(url, { method: "POST", body: slow(["a", "b", "c"], { fail: true }), duplex: "half" })
            .then(() => console.log("failing source: resolved"), (e) => console.log("failing source", e.message));

          const controller = new AbortController();
          setTimeout(() => controller.abort(), 20);
          const never = new ReadableStream({ pull: () => new Promise(() => {}) });
          await fetch(url, { method: "POST", body: never, duplex: "half", signal: controller.signal })
            .then(() => console.log("abort: resolved"), (e) => console.log("abort", e.name));
        } finally {
          server.close();
        }
      });
    "#;
    assert!(matches!(
        runtime.run_embedded_source("fetch_stream_body.js", source),
        RealmExit::Exited(0)
    ));
    assert_eq!(
        String::from_utf8(out.0.borrow().clone()).unwrap().lines().collect::<Vec<_>>(),
        [
            "fetch abcdef",
            "duplex TypeError RequestInit: duplex option is required when sending a body.",
            "request onetwo onetwo",
            "response hello hello",
            "failing source source failed",
            "abort AbortError",
        ],
        "stderr: {}",
        String::from_utf8_lossy(&err.0.borrow())
    );
}
