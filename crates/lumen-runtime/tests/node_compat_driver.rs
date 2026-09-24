//! Node-compat behaviours that a Playwright/`ws`-style client stack depends on: the `http`
//! upgrade handoff, the Buffer numeric family, streaming zlib, Writable/Readable lifecycle
//! semantics, child extra stdio fds, `Timeout` handles, the process end-of-life protocol and the
//! `node:test` runner. Each was found by running a real driver on the runtime, not by reading
//! Node's docs.

use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

use lumen_runtime::{Completion, ConsoleOut, Runtime};

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

impl Captured {
    fn lines(&self) -> Vec<String> {
        String::from_utf8(self.0.borrow().clone())
            .expect("utf8 console output")
            .lines()
            .map(str::to_string)
            .collect()
    }
}

fn runtime() -> (Runtime, Captured) {
    let mut rt = Runtime::new();
    let out = Captured::default();
    rt.engine().ctx().op_state().put(ConsoleOut {
        out: Box::new(out.clone()),
        err: Box::new(Captured::default()),
    });
    (rt, out)
}

fn eval_ok(rt: &mut Runtime, source: &str) {
    match rt.eval(source).expect("script parses") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }
}

#[test]
fn path_resolve_relative_input_is_rooted() {
    let (mut rt, out) = runtime();
    eval_ok(
        &mut rt,
        r#"
        const path = require("node:path");
        const r = path.resolve("a/../b");
        console.log(path.isAbsolute(r), r.endsWith(path.sep + "b"), r.includes(".."));
    "#,
    );
    assert_eq!(out.lines(), ["true true false"]);
}

/// `path.win32` knows drive, UNC and device roots (it used to be the posix algorithm with a
/// backslash). Expectations are Node's own output for the same calls; runs on every platform.
#[test]
fn path_win32_handles_drive_and_unc_roots() {
    let (mut rt, out) = runtime();
    eval_ok(&mut rt, include_str!("fixtures/win32_path.js"));
    let lines = out.lines();
    assert!(
        lines.last().is_some_and(|l| l.ends_with("all ok")),
        "{lines:#?}"
    );
}

#[test]
fn buffer_numeric_family_and_search() {
    let (mut rt, out) = runtime();
    eval_ok(
        &mut rt,
        r#"
        const b = Buffer.alloc(16);
        b.writeUInt16BE(0xabcd, 0);
        b.writeInt32LE(-2, 2);
        b.writeDoubleBE(1.5, 6);
        console.log(b.readUInt16BE(0).toString(16), b.readInt32LE(2), b.readDoubleBE(6));
        b.writeBigUInt64LE(0x1122334455667788n, 8);
        console.log(b.readBigUInt64LE(8).toString(16), b.readUIntBE(0, 3).toString(16), b.readIntLE(2, 4));
        let threw = "";
        try { b.writeUInt8(256, 0); } catch (e) { threw = e.code; }
        console.log(threw);
        const s = Buffer.from("GET / HTTP/1.1\r\nHost: x\r\n\r\nbody");
        console.log(s.indexOf("\r\n\r\n"), s.includes("Host"), s.lastIndexOf("\r\n"), s.indexOf(Buffer.from("body")));
        const w = Buffer.from([1, 2, 3, 4]);
        console.log(w.swap16().toString("hex"), Buffer.alloc(2 * 1024 * 1024).length);
    "#,
    );
    assert_eq!(
        out.lines(),
        [
            "abcd -2 1.5",
            "1122334455667788 abcdfe -2",
            "ERR_OUT_OF_RANGE",
            "23 true 25 27",
            "02010403 2097152",
        ]
    );
}

#[test]
fn http_upgrade_hands_the_socket_over_flowing() {
    let (mut rt, out) = runtime();
    // The peer is a raw `net` server: `http.createServer` is fetch-backed and does not speak
    // Upgrade, and what matters here is the client side — the path `ws` (and so every
    // CDP/DevTools client) takes.
    eval_ok(
        &mut rt,
        r#"
        const http = require("node:http");
        const net = require("node:net");
        const server = net.createServer((socket) => {
          let buffered = "";
          let upgraded = false;
          socket.on("data", (chunk) => {
            if (upgraded) {
              socket.write("echo:" + chunk.toString());
              if (chunk.toString() === "bye") socket.end();
              return;
            }
            buffered += chunk.toString();
            const end = buffered.indexOf("\r\n\r\n");
            if (end < 0) return;
            const head = buffered.slice(0, end);
            console.log("server saw", /^GET \/ HTTP\/1\.1/.test(head), /upgrade: echo/i.test(head), /connection: upgrade/i.test(head));
            upgraded = true;
            socket.write("HTTP/1.1 101 Switching Protocols\r\nUpgrade: echo\r\nConnection: Upgrade\r\n\r\n");
          });
        });
        server.listen(0, "127.0.0.1", () => {
          const req = http.request({
            host: "127.0.0.1", port: server.address().port, path: "/",
            headers: { Connection: "Upgrade", Upgrade: "echo" },
          });
          req.on("response", (res) => console.log("unexpected response", res.statusCode));
          req.on("upgrade", (res, socket, head) => {
            console.log("upgrade", res.statusCode, res.headers.upgrade, socket.readableFlowing, head.length);
            const got = [];
            socket.on("data", (c) => {
              got.push(c.toString());
              if (got.length === 1) socket.write("bye");
            });
            socket.on("close", () => {
              console.log("client", got.join("|"));
              server.close();
            });
            socket.write("hi");
          });
          req.end();
        });
    "#,
    );
    assert_eq!(
        out.lines(),
        [
            "server saw true true true",
            "upgrade 101 echo null 0",
            "client echo:hi|echo:bye",
        ]
    );
}

#[test]
fn stream_lifecycle_matches_node() {
    let (mut rt, out) = runtime();
    eval_ok(
        &mut rt,
        r#"
        const { Readable, Writable, Duplex } = require("node:stream");

        // unshift puts data back at the head of the buffer.
        const r = new Readable({ read() {} });
        r.push("b"); r.unshift("a"); r.push(null);
        const seen = [];
        r.on("data", (c) => seen.push(c.toString()));
        r.on("end", () => console.log("read", seen.join(""), r.readableEnded));
        r.on("close", () => console.log("closed", r.destroyed, r.closed));

        // autoDestroy: 'close' follows 'finish', synchronously-first _write keeps writableLength exact.
        const written = [];
        const w = new Writable({
          write(chunk, enc, cb) { written.push(chunk.toString()); setTimeout(cb, 1); },
        });
        w.write("x");
        console.log("length after first write", w.writableLength);
        w.write("y");
        console.log("length after second write", w.writableLength);
        w.on("finish", () => console.log("finish", w.writableFinished));
        w.on("close", () => console.log("wclose", written.join("")));
        w.end();

        // A Duplex whose readable side ends must not destroy until the writable side finishes.
        r.on("close", () => {
          const d = new Duplex({ read() {}, write(c, e, cb) { cb(); }, });
          d.push(null);
          d.on("end", () => console.log("duplex end", d.destroyed));
          d.on("close", () => console.log("duplex close"));
          d.resume();
          setTimeout(() => d.end(), 50);
        });
    "#,
    );
    assert_eq!(
        out.lines(),
        [
            "length after first write 1",
            "length after second write 2",
            "read ab true",
            "closed true true",
            "duplex end false",
            "finish true",
            "wclose xy",
            "duplex close",
        ]
    );
}

#[test]
fn zlib_streams_flush_with_context_takeover() {
    let (mut rt, out) = runtime();
    eval_ok(
        &mut rt,
        r#"
        const zlib = require("node:zlib");
        const deflate = zlib.createDeflateRaw();
        const inflate = zlib.createInflateRaw();
        const frames = [];
        const decoded = [];
        deflate.on("data", (c) => frames.push(c));
        inflate.on("data", (c) => decoded.push(c.toString()));
        // permessage-deflate: one message per Z_SYNC_FLUSH, tail 00 00 ff ff, window shared across messages.
        deflate.write("hello hello hello");
        deflate.flush(zlib.constants.Z_SYNC_FLUSH, () => {
          const first = Buffer.concat(frames.splice(0));
          console.log("sync tail", first.subarray(-4).toString("hex"));
          inflate.write(first);
          inflate.flush(zlib.constants.Z_SYNC_FLUSH, () => {
            deflate.write("hello hello hello");
            deflate.flush(zlib.constants.Z_SYNC_FLUSH, () => {
              const second = Buffer.concat(frames.splice(0));
              console.log("second smaller", second.length < first.length);
              inflate.write(second);
              inflate.flush(zlib.constants.Z_SYNC_FLUSH, () => {
                console.log("decoded", decoded.join("|"));
                deflate.reset(); inflate.reset();
                console.log("reset ok", typeof deflate.close === "function");
              });
            });
          });
        });
    "#,
    );
    assert_eq!(
        out.lines(),
        [
            "sync tail 0000ffff",
            "second smaller true",
            "decoded hello hello hello|hello hello hello",
            "reset ok true",
        ]
    );
}

#[cfg(unix)]
#[test]
fn child_process_extra_stdio_fds_and_signals() {
    let (mut rt, out) = runtime();
    eval_ok(
        &mut rt,
        r#"
        const { spawn } = require("node:child_process");
        // fd 3 is a socketpair: the child reads one line from it and answers on the same fd.
        const child = spawn("/bin/sh", ["-c", "read line <&3; echo \"got:$line\" >&3"], {
          stdio: ["ignore", "pipe", "pipe", "pipe"],
        });
        const pipe = child.stdio[3];
        pipe.on("data", (c) => console.log("fd3", c.toString().trim()));
        pipe.on("end", () => console.log("fd3 end"));
        child.on("exit", (code) => {
          console.log("exit", code);
          const sleeper = spawn("/bin/sh", ["-c", "sleep 30"]);
          sleeper.on("exit", (c, signal) => console.log("killed", c, signal, sleeper.killed));
          let bad = "";
          try { sleeper.kill("SIGNOPE"); } catch (e) { bad = e.code; }
          console.log("unknown signal", bad);
          console.log("kill", sleeper.kill("SIGTERM"));
        });
        pipe.write("ping\n");
    "#,
    );
    assert_eq!(
        out.lines(),
        [
            "fd3 got:ping",
            "fd3 end",
            "exit 0",
            "unknown signal ERR_UNKNOWN_SIGNAL",
            "kill true",
            "killed null SIGTERM true",
        ]
    );
}

#[test]
fn process_error_hooks_and_timeout_handles() {
    let (mut rt, out) = runtime();
    eval_ok(
        &mut rt,
        r#"
        process.on("unhandledRejection", (reason, promise) => {
          console.log("unhandled", reason.message, promise instanceof Promise);
        });
        Promise.reject(new Error("nobody caught me"));

        // Outlives every ref'd timer: the loop must exit without waiting for it.
        const t = setTimeout(() => console.log("never: unref'd timer must not hold the loop"), 500);
        console.log("hasRef", t.hasRef(), t.unref() === t, t.hasRef(), typeof t.refresh, typeof t[Symbol.toPrimitive]());
        const order = [];
        const a = setTimeout(() => order.push("a"), 10);
        const b = setTimeout(() => { order.push("b"); a.refresh(); }, 5);
        setTimeout(() => console.log("order", order.join(",")), 40);
        let typeErr = "";
        try { setTimeout("not a function", 1); } catch (e) { typeErr = e.code; }
        console.log(typeErr);
    "#,
    );
    assert_eq!(
        out.lines(),
        [
            "hasRef true true false function number",
            "ERR_INVALID_ARG_TYPE",
            "unhandled nobody caught me true",
            "order b,a",
        ]
    );
}

#[test]
fn process_args_exit_protocol_and_exit_code() {
    let (mut rt, out) = runtime();
    rt.set_process_args(
        "/usr/local/bin/lumen",
        &["--expose-gc".to_string()],
        &["script.js".to_string(), "--flag".to_string()],
    );
    rt.expose_gc();
    eval_ok(
        &mut rt,
        r#"
        console.log(process.argv.slice(1).join(" "), "|", process.execArgv.join(" "), "|", typeof gc);
        let rounds = 0;
        process.on("beforeExit", (code) => {
          rounds += 1;
          if (rounds === 1) setTimeout(() => console.log("scheduled from beforeExit"), 1);
          console.log("beforeExit", code, rounds);
        });
        process.on("exit", (code) => console.log("exit", code, process.exitCode));
        process.exitCode = 3;
    "#,
    );
    let code = rt.finish_process();
    assert_eq!(code, 3);
    assert_eq!(
        out.lines(),
        [
            "script.js --flag | --expose-gc | function",
            "beforeExit 3 1",
            "scheduled from beforeExit",
            "beforeExit 3 2",
            "exit 3 3",
        ]
    );
}

#[test]
fn node_test_runner_runs_sequentially_and_reports() {
    let (mut rt, out) = runtime();
    eval_ok(
        &mut rt,
        r#"
        const test = require("node:test");
        const assert = require("node:assert/strict");
        const order = [];
        test.before(() => order.push("before"));
        test.afterEach(() => order.push("afterEach"));
        test("first waits for its promise", async () => {
          await new Promise((r) => setTimeout(r, 5));
          order.push("first");
        });
        test.describe("group", () => {
          test("mocks restore", (t) => {
            const target = { f: () => "real" };
            t.mock.method(target, "f", () => "fake");
            assert.equal(target.f(), "fake");
            t.mock.restoreAll();
            assert.equal(target.f(), "real");
            order.push("second");
          });
          test.skip("skipped", () => order.push("skipped"));
          test("fails", () => { order.push("third"); assert.equal(1, 2); });
        });
        test.after(() => console.log("order", order.join(",")));
        process.on("exit", () => console.log("exitCode", process.exitCode));
    "#,
    );
    let code = rt.finish_process();
    assert_eq!(code, 1);
    let lines = out.lines();
    let summary = lines.join("\n");
    assert!(
        lines.contains(&"order before,first,afterEach,second,afterEach,third,afterEach".to_string()),
        "{summary}"
    );
    assert!(summary.contains("✔ first waits for its promise"), "{summary}");
    assert!(summary.contains("✖ fails"), "{summary}");
    assert!(summary.contains("ℹ tests 4"), "{summary}");
    assert!(summary.contains("ℹ suites 1"), "{summary}");
    assert!(summary.contains("ℹ pass 2"), "{summary}");
    assert!(summary.contains("ℹ fail 1"), "{summary}");
    assert!(summary.contains("ℹ skipped 1"), "{summary}");
    assert!(lines.last().unwrap().ends_with("exitCode 1"), "{summary}");
}

/// `AsyncLocalStorage` rides on the engine's async context: the store set by `run` is visible
/// from promise reactions, `await` continuations, `queueMicrotask`, `nextTick`, timers and
/// `setImmediate`, and an I/O completion that was not scheduled inside a `run` sees no store.
/// The extension host scopes each extension's `vscode` API by it.
#[test]
fn async_local_storage_propagates_across_async_boundaries() {
    let (mut rt, out) = runtime();
    eval_ok(
        &mut rt,
        r#"
        const { AsyncLocalStorage, AsyncResource } = require("node:async_hooks");
        const als = new AsyncLocalStorage();
        const seen = [];
        const note = (label) => seen.push(label + "=" + als.getStore());
        als.run("A", async () => {
          Promise.resolve().then(() => note("then"));
          queueMicrotask(() => note("microtask"));
          process.nextTick(() => note("nextTick"));
          setTimeout(() => note("timer"), 1);
          setImmediate(() => note("immediate"));
          await null;
          note("await");
          als.run("B", () => { note("nested"); setTimeout(() => note("nestedTimer"), 1); });
          note("afterNested");
          als.exit(() => note("exit"));
          const bound = AsyncResource.bind(() => note("resource"));
          setTimeout(() => als.run("C", bound), 1);
        });
        note("outside");
        setTimeout(() => {
          note("outsideTimer");
          console.log(seen.sort().join(","));
        }, 20);
    "#,
    );
    rt.run_to_completion();
    assert_eq!(
        out.lines(),
        ["afterNested=A,await=A,exit=undefined,immediate=A,microtask=A,nested=B,nestedTimer=B,nextTick=A,outside=undefined,outsideTimer=undefined,resource=A,then=A,timer=A"]
    );
}

/// `writeFileSync` honours `{ flag: "wx", mode }`: exclusive creation fails with an `EEXIST`-coded
/// error and the created file carries the requested permission bits. The capture driver installs
/// its browser shim this way and asserts the mode.
#[test]
fn write_file_exclusive_create_and_mode() {
    let (mut rt, out) = runtime();
    eval_ok(
        &mut rt,
        r##"
        const fs = require("node:fs"), os = require("node:os"), path = require("node:path");
        const dir = fs.mkdtempSync(path.join(os.tmpdir(), "lumen-wx-"));
        const file = path.join(dir, "shim");
        fs.writeFileSync(file, "#!/bin/sh\n", { flag: "wx", mode: 0o700 });
        const mode = process.platform === "win32" ? "n/a" : (fs.statSync(file).mode & 0o777).toString(8);
        let second = "no throw";
        try { fs.writeFileSync(file, "x", { flag: "wx" }); } catch (e) { second = e.code + " " + e.syscall + " " + (e.path === file); }
        let numeric = "no throw";
        try { fs.openSync(file, fs.constants.O_WRONLY | fs.constants.O_CREAT | fs.constants.O_EXCL); } catch (e) { numeric = e.code; }
        fs.rmSync(dir, { recursive: true });
        console.log(mode, second, numeric, fs.existsSync(file));
    "##,
    );
    let expected = if cfg!(windows) { "n/a EEXIST open true EEXIST false" } else { "700 EEXIST open true EEXIST false" };
    assert_eq!(out.lines(), [expected]);
}

/// A write issued while the socket is still connecting is held until 'connect' and then sent in
/// order (Node buffers it); a failed connect fails the held write through the socket's error,
/// not a stray "ended by the other party".
#[test]
fn net_write_before_connect_waits_for_the_connection() {
    let (mut rt, out) = runtime();
    eval_ok(
        &mut rt,
        r#"
        const net = require("node:net");
        const server = net.createServer((conn) => {
          let got = "";
          conn.on("data", (c) => { got += c; if (got === "firstsecond") { conn.end("ok"); } });
        });
        server.listen(0, "127.0.0.1", () => {
          const client = net.connect({ host: "127.0.0.1", port: server.address().port });
          client.write("first");
          client.write("second", () => console.log("second written, connecting:", client.connecting));
          client.on("data", (c) => console.log("reply", String(c)));
          client.on("close", () => server.close());
        });
        const dead = net.connect({ host: "127.0.0.1", port: 1 });
        dead.write("never", (err) => console.log("held write failed:", err.code));
        dead.on("error", (err) => console.log("connect failed:", err.code));
    "#,
    );
    let mut lines = out.lines();
    lines.sort();
    assert_eq!(
        lines,
        [
            "connect failed: ECONNREFUSED",
            "held write failed: ERR_SOCKET_CLOSED_BEFORE_CONNECTION",
            "reply ok",
            "second written, connecting: false",
        ]
    );
}

/// A request ended in the tick it was made: its DATA frame must go out after the connection
/// preface, SETTINGS and HEADERS, not ahead of them (which the peer answers with a reset).
#[test]
fn http2_request_ended_before_connect_keeps_frame_order() {
    let (mut rt, out) = runtime();
    eval_ok(
        &mut rt,
        r#"
        const http2 = require("node:http2");
        const server = http2.createServer();
        server.on("stream", (stream, headers) => {
          let body = "";
          stream.on("data", (c) => body += c);
          stream.on("end", () => { stream.respond({ ":status": 200 }); stream.end(headers[":path"] + ":" + body); });
        });
        server.on("session", (session) => session.on("close", () => server.close()));
        server.listen(0, "127.0.0.1", () => {
          const client = http2.connect("http://127.0.0.1:" + server.address().port);
          client.on("error", (e) => console.log("client error", e.message));
          let done = 0;
          const request = (path, body) => {
            const req = client.request({ ":method": body ? "POST" : "GET", ":path": path });
            let data = "";
            req.on("response", (h) => console.log("response", h[":status"]));
            req.on("data", (c) => data += c);
            req.on("end", () => { console.log("body", data); if (++done === 2) client.close(); });
            req.end(body);
          };
          request("/first");
          request("/second", "hello");
        });
    "#,
    );
    let mut lines = out.lines();
    lines.sort();
    assert_eq!(lines, ["body /first:", "body /second:hello", "response 200", "response 200"]);
}
