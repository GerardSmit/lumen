use std::cell::RefCell;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::rc::Rc;
use std::sync::mpsc::{self, Sender};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use lumen_runtime::{Completion, ConsoleOut, Runtime};
use lumen_web::FetchConfig;

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

struct LineSender {
    sender: Sender<String>,
    pending: Vec<u8>,
}

impl Write for LineSender {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.pending.extend_from_slice(bytes);
        while let Some(newline) = self.pending.iter().position(|byte| *byte == b'\n') {
            let line = self.pending.drain(..=newline).collect::<Vec<_>>();
            let line = String::from_utf8_lossy(&line[..line.len() - 1]).into_owned();
            let _ = self.sender.send(line);
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if !self.pending.is_empty() {
            let line = String::from_utf8_lossy(&std::mem::take(&mut self.pending)).into_owned();
            let _ = self.sender.send(line);
        }
        Ok(())
    }
}

#[test]
fn worker_threads_exchange_structured_messages() {
    let mut runtime = Runtime::new();
    let out = Captured::default();
    runtime.engine().ctx().op_state().put(ConsoleOut {
        out: Box::new(out.clone()),
        err: Box::new(Captured::default()),
    });

    let source = r#"
        const { Worker } = require("node:worker_threads");
        const worker = new Worker(`
            const { parentPort, workerData, isMainThread, threadId } = require("node:worker_threads");
            parentPort.on("message", message => {
                parentPort.postMessage({
                    answer: message.value + workerData.offset,
                    worker: !isMainThread && threadId > 0,
                });
                parentPort.close();
            });
        `, { eval: true, workerData: { offset: 2 } });
        worker.on("online", () => worker.postMessage({ value: 40 }));
        worker.on("message", message => console.log("message", JSON.stringify(message)));
        worker.on("exit", code => console.log("exit", code));
    "#;
    match runtime.eval(source).expect("source parses") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }

    assert_eq!(
        out.lines(),
        ["message {\"answer\":42,\"worker\":true}", "exit 0"]
    );
}

#[test]
fn same_realm_node_ports_queue_poll_emit_and_ref_genuinely() {
    let mut runtime = Runtime::new();
    let out = Captured::default();
    runtime.engine().ctx().op_state().put(ConsoleOut {
        out: Box::new(out.clone()),
        err: Box::new(Captured::default()),
    });
    let source = r#"
        const assert = require('node:assert/strict');
        const { MessageChannel, MessagePort, receiveMessageOnPort } = require('node:worker_threads');
        const { port1, port2 } = new MessageChannel();
        assert(port1 instanceof MessagePort);
        assert(port1 instanceof EventTarget);
        assert.equal(port1.hasRef(), false);
        const initial = { value: 1 };
        port2.postMessage(initial);
        initial.value = 99;
        assert.deepEqual(receiveMessageOnPort(port1), { message: { value: 1 } });
        assert.equal(receiveMessageOnPort(port1), undefined);
        port1.on('message', function (message) {
            assert.equal(this, port1);
            assert.equal(message.value, 2);
            port1.close();
            assert.equal(port1.hasRef(), false);
            console.log('received');
        });
        assert.equal(port1.hasRef(), true);
        port1.unref(); assert.equal(port1.hasRef(), false);
        port1.ref(); assert.equal(port1.hasRef(), true);
        port1.once('close', () => console.log('closed first'));
        port2.once('close', () => console.log('closed second'));
        port2.postMessage({ value: 2 });
    "#;
    match runtime.eval(source).expect("source parses") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }
    assert_eq!(out.lines(), ["received", "closed first", "closed second"]);
}

#[test]
fn node_and_web_message_ports_keep_their_event_target_after_dom_install() {
    let mut runtime = Runtime::new();
    let out = Captured::default();
    let err = Captured::default();
    runtime.engine().ctx().op_state().put(ConsoleOut {
        out: Box::new(out.clone()),
        err: Box::new(err.clone()),
    });

    // Load Node's MessageChannel before the DOM adapter replaces the global EventTarget.
    match runtime
        .eval("globalThis.__nodeMessageChannelForDomTest = require('node:worker_threads').MessageChannel;")
        .expect("Node channel setup parses")
    {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }
    let _realm = lumen_html_js::install(runtime.engine().ctx(), "<body></body>", 64).unwrap();
    runtime.set_deadline(Duration::from_secs(10));

    let source = r#"
        const assert = require('node:assert/strict');
        const NodeMessageChannel = globalThis.__nodeMessageChannelForDomTest;
        const domEventTarget = EventTarget;
        const nodeChannel = new NodeMessageChannel();
        const nodeSource = nodeChannel.port1;
        const nodeTarget = nodeChannel.port2;
        let nodeSeen = false;
        let webSeen = false;
        const maybeReport = () => {
            if (nodeSeen && webSeen) {
                assert.equal(EventTarget, domEventTarget);
                console.log('node and web message targets preserved');
            }
        };
        nodeTarget.addEventListener('message', event => {
            console.log('node event target: ' + (event.target === nodeTarget));
            assert.equal(event.target, nodeTarget);
            nodeSeen = true;
            nodeSource.close();
            nodeTarget.close();
            maybeReport();
        });
        nodeTarget.start();
        nodeSource.postMessage('node');

        // This hidden lazy trigger installs browser MessageChannel without replacing the DOM
        // EventTarget that the native document adapter just exposed.
        const portBridge = globalThis.__lumenSharedPorts;
        assert.equal(EventTarget, domEventTarget);
        assert.equal(typeof portBridge.create, 'function');
        const webChannel = new MessageChannel();
        const webSource = webChannel.port1;
        const webTarget = webChannel.port2;
        webTarget.addEventListener('message', event => {
            console.log('web event target: ' + (event.target === webTarget));
            assert.equal(event.target, webTarget);
            webSeen = true;
            webSource.close();
            webTarget.close();
            maybeReport();
        });
        webTarget.start();
        webSource.postMessage('web');
    "#;
    match runtime.eval(source).expect("mixed message-port source parses") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!(
            "uncaught {name}: {message}; stdout: {:?}; stderr: {:?}",
            out.lines(),
            err.lines()
        ),
    }
    assert!(
        !runtime.is_interrupted(),
        "mixed Node/web MessageChannel exchange did not settle; stdout: {:?}; stderr: {:?}",
        out.lines(),
        err.lines()
    );
    let lines = out.lines();
    assert!(
        lines.iter().any(|line| line == "node event target: true"),
        "Node port event target mismatch or callback missing; stdout: {lines:?}; stderr: {:?}",
        err.lines()
    );
    assert!(
        lines.iter().any(|line| line == "web event target: true"),
        "web port event target mismatch or callback missing; stdout: {lines:?}; stderr: {:?}",
        err.lines()
    );
    assert!(
        lines
            .iter()
            .any(|line| line == "node and web message targets preserved"),
        "mixed message-port exchange did not complete; stdout: {lines:?}; stderr: {:?}",
        err.lines()
    );
}

#[test]
fn node_message_ports_first_required_after_dom_use_node_event_target() {
    let mut runtime = Runtime::new();
    let out = Captured::default();
    let err = Captured::default();
    runtime.engine().ctx().op_state().put(ConsoleOut {
        out: Box::new(out.clone()),
        err: Box::new(err.clone()),
    });
    let _realm = lumen_html_js::install(runtime.engine().ctx(), "<body></body>", 64).unwrap();
    runtime.set_deadline(Duration::from_secs(10));

    let source = r#"
        const assert = require('node:assert/strict');
        const domEventTarget = EventTarget;
        const { MessageChannel } = require('node:worker_threads');
        const channel = new MessageChannel();
        channel.port2.addEventListener('message', event => {
            console.log('node-after-dom target: ' + (event.target === channel.port2));
            assert.equal(event.target, channel.port2);
            assert.equal(EventTarget, domEventTarget);
            channel.port1.close();
            channel.port2.close();
            console.log('node-after-dom exchange complete');
        });
        channel.port2.start();
        channel.port1.postMessage('node');
    "#;
    match runtime.eval(source).expect("after-DOM Node port source parses") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!(
            "uncaught {name}: {message}; stdout: {:?}; stderr: {:?}",
            out.lines(),
            err.lines()
        ),
    }
    assert!(
        !runtime.is_interrupted(),
        "after-DOM Node MessageChannel exchange did not settle; stdout: {:?}; stderr: {:?}",
        out.lines(),
        err.lines()
    );
    let lines = out.lines();
    assert!(
        lines.iter().any(|line| line == "node-after-dom target: true"),
        "Node MessageEvent target mismatch or callback missing; stdout: {lines:?}; stderr: {:?}",
        err.lines()
    );
    assert!(
        lines.iter().any(|line| line == "node-after-dom exchange complete"),
        "after-DOM Node MessageChannel exchange did not complete; stdout: {lines:?}; stderr: {:?}",
        err.lines()
    );
}

#[test]
fn transferred_port_keeps_queued_attachment_and_shared_atomic_decision() {
    let mut runtime = Runtime::new();
    let out = Captured::default();
    runtime.engine().ctx().op_state().put(ConsoleOut {
        out: Box::new(out.clone()),
        err: Box::new(Captured::default()),
    });
    let source = r#"
      const assert=require('node:assert/strict');
      const {Worker,MessageChannel,MessagePort,receiveMessageOnPort}=require('node:worker_threads');
      const channel=new MessageChannel();
      channel.port1.postMessage({queued:41});
      channel.port1.on('message', ({decision})=>{
        const view=new Int32Array(decision);
        assert.equal(Atomics.compareExchange(view,0,0,7),0);
        Atomics.notify(view,0);
      });
      const worker=new Worker(`
        const assert=require('node:assert/strict');
        const {parentPort,MessagePort,receiveMessageOnPort}=require('node:worker_threads');
        parentPort.on('message', ({port})=>{
          assert(port instanceof MessagePort);
          assert.deepEqual(receiveMessageOnPort(port),{message:{queued:41}});
          const decision=new SharedArrayBuffer(4);
          const view=new Int32Array(decision);
          port.postMessage({decision});
          while(Atomics.load(view,0)===0) Atomics.wait(view,0,0,3000);
          const bytes=new Uint8Array([1,128,255]);
          parentPort.postMessage({value:Atomics.load(view,0),bytes},[bytes.buffer]);
          assert.equal(bytes.byteLength,0);
          port.close();parentPort.close();
        });
      `,{eval:true});
      worker.on('error',error=>{throw error;});
      worker.on('message',message=>{assert.equal(message.value,7);assert.deepEqual(Array.from(message.bytes),[1,128,255]);channel.port1.close();console.log('shared decision');});
      worker.on('exit',code=>{assert.equal(code,0);console.log('exit');});
      worker.postMessage({port:channel.port2},[channel.port2]);
      assert.equal(channel.port2.hasRef(),false);
    "#;
    match runtime.eval(source).expect("source parses") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }
    assert_eq!(out.lines(), ["shared decision", "exit"]);
}

#[test]
fn failed_port_clone_preserves_ownership_and_marks_are_enforced() {
    let mut runtime = Runtime::new();
    let source = r#"
 const assert=require('node:assert/strict');
 const {MessageChannel,receiveMessageOnPort,markAsUntransferable,isMarkedAsUntransferable,markAsUncloneable}=require('node:worker_threads');
 const pair=new MessageChannel();
 const carrier=new MessageChannel();
 assert.throws(()=>carrier.port1.postMessage({port:pair.port2,fn(){ }},[pair.port2]),{name:'DataCloneError'});
 pair.port1.postMessage(42);
 assert.deepEqual(receiveMessageOnPort(pair.port2),{message:42});
 assert.throws(()=>carrier.port1.postMessage({port:pair.port2}),{code:'ERR_MISSING_TRANSFERABLE_IN_TRANSFER_LIST'});
 assert.throws(()=>carrier.port1.postMessage({port:pair.port2},[pair.port2,pair.port2]),{name:'DataCloneError'});
 markAsUntransferable(pair.port2);assert(isMarkedAsUntransferable(pair.port2));
 assert.throws(()=>carrier.port1.postMessage({port:pair.port2},[pair.port2]),{name:'DataCloneError'});
 const v8=require('node:v8');
 assert.throws(()=>v8.serialize(new SharedArrayBuffer(4)));
 assert.throws(()=>v8.serialize(pair.port1));
 const value={ok:true};markAsUncloneable(value);
 assert.throws(()=>pair.port1.postMessage(value),{name:'DataCloneError'});
 pair.port1.close();carrier.port1.close();
 "#;
    match runtime.eval(source).expect("source parses") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }
}

#[test]
fn nested_post_from_getter_preserves_outer_transfer_capabilities() {
    let mut runtime = Runtime::new();
    let source = r#"
 const assert=require('node:assert/strict');
 const {MessageChannel,receiveMessageOnPort}=require('node:worker_threads');
 const outer=new MessageChannel();const nested=new MessageChannel();const attached=new MessageChannel();
 attached.port1.postMessage(17);
 outer.port1.postMessage({port:attached.port2,get nested(){nested.port1.postMessage({shared:new SharedArrayBuffer(4)});return 1;}},[attached.port2]);
 const result=receiveMessageOnPort(outer.port2).message;
 assert.equal(result.nested,1);
 assert.deepEqual(receiveMessageOnPort(result.port),{message:17});
 const inner=receiveMessageOnPort(nested.port2).message;
 assert(inner.shared instanceof SharedArrayBuffer);
 outer.port1.close();nested.port1.close();attached.port1.close();result.port.close();
 "#;
    match runtime.eval(source).expect("source parses") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }
}

#[test]
fn buffer_and_subclass_clones_use_intrinsic_view_brand_and_offsets() {
    let mut runtime = Runtime::new();
    let source = r#"
 const assert=require('node:assert/strict');
 const {MessageChannel,receiveMessageOnPort}=require('node:worker_threads');
 const pair=new MessageChannel();
 const buffer=Buffer.from([1,2,3,4]).subarray(1,3);
 Object.defineProperty(buffer,'constructor',{get(){throw new Error('poisoned constructor');}});
 class Custom extends Uint16Array {}
 const backing=new SharedArrayBuffer(12);
 const custom=new Custom(backing,2,3);custom.set([257,513,1025]);
 Object.defineProperty(custom,'constructor',{value:{name:'Float64Array'}});
 pair.port1.postMessage({buffer,custom,alias:custom,backing,view:new DataView(backing,2,6)});
 const message=receiveMessageOnPort(pair.port2).message;
 assert(message.buffer instanceof Uint8Array);assert.equal(Buffer.isBuffer(message.buffer),false);
 assert.deepEqual(Array.from(message.buffer),[2,3]);
 assert.equal(message.alias,message.custom);assert.equal(message.backing,message.custom.buffer);assert.equal(message.view.buffer,message.backing);
 assert(message.custom instanceof Uint16Array);assert.equal(message.custom.byteOffset,2);
 assert.equal(message.custom.length,3);assert(message.custom.buffer instanceof SharedArrayBuffer);
 assert.deepEqual(Array.from(message.custom),[257,513,1025]);
 Atomics.store(message.custom,0,771);assert.equal(custom[0],771);
 pair.port1.close();
 "#;
    match runtime.eval(source).expect("source parses") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }
}

#[test]
fn array_buffer_transfer_detaches_sender_and_preserves_view_aliases() {
    let mut runtime = Runtime::new();
    let source = r#"
 const assert=require('node:assert/strict');
 const {MessageChannel,receiveMessageOnPort}=require('node:worker_threads');
 const pair=new MessageChannel();
 const buffer=new ArrayBuffer(8);const view=new Uint8Array(buffer,2,3);view.set([1,128,255]);
 const bytes=new Uint8Array(buffer);
 pair.port1.postMessage({buffer,view,alias:view,bytes},[buffer]);
 assert.equal(buffer.byteLength,0);assert.equal(view.byteLength,0);
 const result=receiveMessageOnPort(pair.port2).message;
 assert.equal(result.buffer.byteLength,8);assert.equal(result.view.byteOffset,2);assert.equal(result.view.length,3);
 assert.equal(result.alias,result.view);assert.equal(result.view.buffer,result.buffer);assert.equal(result.bytes.buffer,result.buffer);
 assert.deepEqual(Array.from(result.view),[1,128,255]);
 const invalid=new ArrayBuffer(2);
 assert.throws(()=>pair.port1.postMessage({fn(){}},[invalid]),{name:'DataCloneError'});assert.equal(invalid.byteLength,2);
 assert.throws(()=>pair.port1.postMessage({},[invalid,invalid]),{name:'DataCloneError'});assert.equal(invalid.byteLength,2);
 const shared=new SharedArrayBuffer(2);assert.throws(()=>pair.port1.postMessage({},[shared]),{name:'DataCloneError'});
 const detached=new ArrayBuffer(2);pair.port1.postMessage({},[detached]);assert.equal(detached.byteLength,0);
 assert.throws(()=>pair.port1.postMessage({},[detached]),{name:'DataCloneError'});
 pair.port1.close();
 "#;
    match runtime.eval(source).expect("source parses") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }
}

#[test]
fn message_port_delivers_queued_receipt_before_peer_close() {
    let mut runtime = Runtime::new();
    let out = Captured::default();
    runtime.engine().ctx().op_state().put(ConsoleOut {
        out: Box::new(out.clone()),
        err: Box::new(Captured::default()),
    });
    runtime
        .eval(
            r#"
        const {MessageChannel}=require('node:worker_threads');
        const {port1,port2}=new MessageChannel();
        port1.on('message',value=>console.log('message',value));
        port1.on('close',()=>console.log('close'));
        port2.postMessage('receipt');
        port2.close();
    "#,
        )
        .expect("parse");
    assert_eq!(out.lines(), ["message receipt", "close"]);
}

#[test]
fn shared_worker_connects_clients_transfers_ports_and_closes_last_client() {
    static NEXT_SCRIPT: AtomicU64 = AtomicU64::new(1);
    let script_path = std::env::temp_dir().join(format!(
        "lumen-shared-worker-{}-{}.js",
        std::process::id(),
        NEXT_SCRIPT.fetch_add(1, Ordering::Relaxed),
    ));
    std::fs::write(
        &script_path,
        r#"
          let connectionCount = 0;
          globalThis.onconnect = ({ ports }) => {
            const id = ++connectionCount;
            const port = ports[0];
            port.onmessage = ({ data }) => {
              if (data.kind === "channel") {
                const transferred = data.port;
                transferred.onmessage = ({ data: value }) => transferred.postMessage({ id, value });
                transferred.postMessage({ id, ready: true });
              } else {
                port.postMessage({ id, value: data.value });
              }
            };
          };
        "#,
    )
    .expect("write shared worker script");
    let url = format!("file://{}", script_path.to_string_lossy());
    let source = format!(
        r#"
          const assert = require("node:assert/strict");
          const url = {url:?};
          const first = new SharedWorker(url, {{ name: "shared-instance" }});
          const second = new SharedWorker(url, {{ name: "shared-instance" }});
          const channel = new MessageChannel();
          let received = 0;
          let transferDone = false;
          const finish = () => {{
            if (received !== 2 || !transferDone) return;
            first.port.close();
            second.port.close();
            console.log("shared clients");
          }};
          first.port.onmessage = ({{ data }}) => {{
            assert.deepEqual(data, {{ id: 1, value: 41 }});
            received++;
            finish();
          }};
          second.port.onmessage = ({{ data }}) => {{
            assert.deepEqual(data, {{ id: 2, value: 42 }});
            received++;
            finish();
          }};
          channel.port1.onmessage = ({{ data }}) => {{
            if (data.ready) {{
              assert.equal(data.id, 1);
              channel.port1.postMessage("transferred ping");
            }} else {{
              assert.deepEqual(data, {{ id: 1, value: "transferred ping" }});
              channel.port1.close();
              transferDone = true;
              console.log("transferred port");
              finish();
            }}
          }};
          first.port.postMessage({{ value: 41 }});
          second.port.postMessage({{ value: 42 }});
          first.port.postMessage({{ kind: "channel", port: channel.port2 }}, [channel.port2]);
        "#
    );

    let mut runtime = Runtime::new();
    let out = Captured::default();
    runtime.engine().ctx().op_state().put(ConsoleOut {
        out: Box::new(out.clone()),
        err: Box::new(Captured::default()),
    });
    runtime.set_deadline(Duration::from_secs(10));
    let result = runtime.eval(&source).expect("shared worker source parses");
    if let Completion::Throw { name, message } = result {
        panic!("uncaught {name}: {message}");
    }
    assert!(!runtime.is_interrupted(), "shared-worker port exchange did not settle");
    let mut lines = out.lines();
    lines.sort();
    assert_eq!(lines, ["shared clients", "transferred port"]);
    std::fs::remove_file(script_path).expect("remove shared worker script");
}

#[test]
fn shared_worker_registry_is_shared_across_runtime_instances() {
    static NEXT_SCRIPT: AtomicU64 = AtomicU64::new(1);
    let script_path = std::env::temp_dir().join(format!(
        "lumen-shared-runtime-{}-{}.js",
        std::process::id(),
        NEXT_SCRIPT.fetch_add(1, Ordering::Relaxed),
    ));
    std::fs::write(
        &script_path,
        r#"
          let connectionCount = 0;
          globalThis.onconnect = ({ ports }) => {
            const id = ++connectionCount;
            const port = ports[0];
            port.onmessage = ({ data }) => port.postMessage({ id, data });
          };
        "#,
    )
    .expect("write shared worker script");
    let url = format!("file://{}", script_path.to_string_lossy());
    let (line_sender, lines) = mpsc::channel::<String>();

    let spawn_runtime = |label: &'static str, delay_ms: u64, url: String, output: Sender<String>| {
        thread::spawn(move || {
            let source = format!(
                r#"
                  const worker = new SharedWorker({url:?}, {{ name: "cross-runtime" }});
                  const watchdog = setTimeout(() => worker.port.close(), 5000);
                  worker.port.onmessage = ({{ data }}) => {{
                    console.log("result-{label}", data.id, data.data);
                    clearTimeout(watchdog);
                    worker.port.close();
                  }};
                  setTimeout(() => worker.port.postMessage("{label}"), {delay_ms});
                  console.log("ready-{label}");
                "#
            );
            let mut runtime = Runtime::new();
            runtime.set_deadline(Duration::from_secs(10));
            runtime.engine().ctx().op_state().put(ConsoleOut {
                out: Box::new(LineSender { sender: output.clone(), pending: Vec::new() }),
                err: Box::new(LineSender { sender: output, pending: Vec::new() }),
            });
            match runtime.eval(&source).expect("shared worker source parses") {
                Completion::Value(_) => {}
                Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
            }
            assert!(!runtime.is_interrupted(), "shared-worker runtime exchange did not settle");
        })
    };

    let first = spawn_runtime("first", 1500, url.clone(), line_sender.clone());
    assert_eq!(lines.recv_timeout(Duration::from_secs(10)).expect("first ready"), "ready-first");
    let second = spawn_runtime("second", 100, url, line_sender);
    assert_eq!(lines.recv_timeout(Duration::from_secs(10)).expect("second ready"), "ready-second");
    first.join().expect("first runtime exits");
    second.join().expect("second runtime exits");

    let mut results = Vec::new();
    while let Ok(line) = lines.try_recv() {
        if line.starts_with("result-") { results.push(line); }
    }
    results.sort();
    assert_eq!(results, ["result-first 1 first", "result-second 2 second"]);
    std::fs::remove_file(script_path).expect("remove shared worker script");
}

#[test]
fn shared_worker_http_redirect_loads_module_dependencies_from_final_url() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind HTTP test server");
    listener.set_nonblocking(true).expect("set nonblocking HTTP listener");
    let address = listener.local_addr().expect("HTTP test server address");
    let server = thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        for _ in 0..3 {
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(std::time::Instant::now() < deadline, "timed out waiting for module request");
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept HTTP module request: {error}"),
                }
            };
            let mut request = Vec::new();
            let mut chunk = [0; 1024];
            loop {
                let count = stream.read(&mut chunk).expect("read HTTP request");
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..count]);
                if request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    break;
                }
            }
            let first_line = String::from_utf8_lossy(&request)
                .lines()
                .next()
                .expect("HTTP request line")
                .to_owned();
            let path = first_line.split_whitespace().nth(1).expect("request path");
            let response = match path {
                "/redirect" => {
                    b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 302 Found\r\nLocation: /modules/entry.mjs\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
                }
                "/modules/entry.mjs" => {
                    let body = b"import { answer } from './dependency.mjs'; const checks = { scope: self instanceof WorkerGlobalScope, shared: self instanceof SharedWorkerGlobalScope, dedicatedInterfaceAbsent: typeof DedicatedWorkerGlobalScope === 'undefined', eventTarget: self instanceof EventTarget, workerLocation: location instanceof WorkerLocation, href: location.href, name: name, type: type, moduleImportScriptsThrows: (() => { try { importScripts('/forbidden.js'); return false; } catch (error) { return error instanceof TypeError; } })() }; let connection = 0; onconnect = function(event) { const port = event.ports[0]; port.postMessage({ answer, id: ++connection, checks, sourceMatchesPort: event.source === port, eventTargetMatchesGlobal: event.currentTarget === self, handlerThisIsGlobal: this === self, portsFrozen: Object.isFrozen(event.ports), trusted: event.isTrusted }); };";
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/javascript; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .into_bytes()
                    .into_iter()
                    .chain(body.iter().copied())
                    .collect()
                }
                "/modules/dependency.mjs" => {
                    let body = b"export const answer = 'redirected dependency';";
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/javascript\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .into_bytes()
                    .into_iter()
                    .chain(body.iter().copied())
                    .collect()
                }
                other => panic!("unexpected module request: {other}"),
            };
            stream.write_all(&response).expect("write HTTP response");
        }
    });

    let mut runtime = Runtime::new();
    runtime.set_deadline(Duration::from_secs(10));
    let out = Captured::default();
    runtime.engine().ctx().op_state().put(ConsoleOut {
        out: Box::new(out.clone()),
        err: Box::new(Captured::default()),
    });
    let final_url = format!("http://{address}/modules/entry.mjs");
    let source = format!(
        r#"
          const worker = new SharedWorker("http://{address}/redirect", {{ type: "module", name: "http-module" }});
          let finalWorker;
          let messages = 0;
          const receive = ({{ data }}) => {{
            const id = messages + 1;
            if (data.answer !== "redirected dependency" || data.id !== id) throw new Error(`unexpected message: ${{JSON.stringify(data)}}`);
            const checks = data.checks;
            if (!checks.scope || !checks.shared || checks.dedicatedInterfaceAbsent !== true || !checks.eventTarget || !checks.workerLocation || checks.href !== {final_url:?} || checks.name !== "http-module" || checks.type !== "module" || !checks.moduleImportScriptsThrows || !data.sourceMatchesPort || !data.eventTargetMatchesGlobal || !data.handlerThisIsGlobal || !data.portsFrozen || !data.trusted) throw new Error(`invalid shared module scope: ${{JSON.stringify(data)}}`);
            messages = id;
            if (messages === 1) {{
              finalWorker = new SharedWorker({final_url:?}, {{ type: "module", name: "http-module" }});
              finalWorker.port.onmessage = receive;
            }} else {{
              worker.port.close();
              finalWorker.port.close();
              console.log("http shared worker");
            }}
          }};
          worker.port.onmessage = receive;
        "#
    );
    match runtime.eval(&source).expect("HTTP SharedWorker source parses") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }
    assert!(!runtime.is_interrupted(), "shared-worker module exchange did not settle");
    server.join().expect("HTTP module server exits");
    assert_eq!(out.lines(), ["http shared worker"]);
}

#[test]
fn shared_worker_http_classic_has_shared_scope_and_real_connect_ports() {
    fn accept_request(
        listener: &TcpListener,
        deadline: std::time::Instant,
    ) -> (std::net::TcpStream, String) {
        let (mut stream, _) = loop {
            match listener.accept() {
                Ok(connection) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(std::time::Instant::now() < deadline, "timed out waiting for shared-worker HTTP request");
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("accept shared-worker HTTP request: {error}"),
            }
        };
        stream.set_nonblocking(false).expect("set shared-worker request blocking mode");
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .expect("set shared-worker request timeout");
        let mut request = Vec::new();
        let mut chunk = [0; 1024];
        loop {
            let count = stream.read(&mut chunk).expect("read shared-worker request");
            if count == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..count]);
            if request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                break;
            }
        }
        let first_line = String::from_utf8_lossy(&request)
            .lines()
            .next()
            .expect("shared-worker request line")
            .to_owned();
        let path = first_line
            .split_whitespace()
            .nth(1)
            .expect("shared-worker request path")
            .to_owned();
        (stream, path)
    }

    fn send_script(stream: &mut std::net::TcpStream, body: &str) {
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/javascript; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .expect("write shared-worker JavaScript response");
    }

    let worker_listener = TcpListener::bind("127.0.0.1:0").expect("bind shared-worker fixture");
    worker_listener.set_nonblocking(true).expect("set shared-worker fixture nonblocking");
    let worker_address = worker_listener.local_addr().expect("shared-worker fixture address");
    let worker_port = worker_address.port();
    let import_listener = TcpListener::bind("127.0.0.1:0").expect("bind shared import fixture");
    import_listener.set_nonblocking(true).expect("set shared import fixture nonblocking");
    let import_address = import_listener.local_addr().expect("shared import fixture address");
    let import_port = import_address.port();

    let worker_server = thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let (mut redirect, path) = accept_request(&worker_listener, deadline);
        assert_eq!(path, "/redirect.js?case=1");
        redirect
            .write_all(b"HTTP/1.1 302 Found\r\nLocation: /dir/entry.js?case=1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .expect("write shared-worker redirect");

        let (mut entry, path) = accept_request(&worker_listener, deadline);
        assert_eq!(path, "/dir/entry.js?case=1");
        let body = format!(
            r#"
            importScripts("http://alt.test:{import_port}/lib.js?lib=1");
            const initialName = name;
            const initialNameIsInherited = !Object.prototype.hasOwnProperty.call(self, "name");
            self.name = "replacement";
            const nameReplacementWorks = self.name === "replacement" && Object.prototype.hasOwnProperty.call(self, "name");
            delete self.name;
            const nameRestored = name === initialName && !Object.prototype.hasOwnProperty.call(self, "name");
            const connectListenerState = {{ count: 0, receiver: false, currentTarget: false }};
            self.addEventListener("connect", function (event) {{
              connectListenerState.count++;
              connectListenerState.receiver = this === self;
              connectListenerState.currentTarget = event.currentTarget === self;
            }});
            onconnect = function (event) {{
              const port = event.ports[0];
              const report = {{
                scope: self instanceof WorkerGlobalScope,
                shared: self instanceof SharedWorkerGlobalScope,
                dedicatedInterfaceAbsent: typeof DedicatedWorkerGlobalScope === "undefined",
                eventTarget: self instanceof EventTarget,
                workerLocation: location instanceof WorkerLocation,
                href: location.href,
                origin: location.origin,
                type,
                name: initialName,
                imported: globalThis.crossOriginImport,
                initialNameIsInherited,
                nameReplacementWorks,
                nameRestored,
                sourceMatchesPort: event.source === port,
                eventTargetMatchesGlobal: event.currentTarget === self,
                handlerThisIsGlobal: this === self,
                trusted: event.isTrusted,
                portsFrozen: Object.isFrozen(event.ports),
                connectListenerCount: connectListenerState.count,
                connectListenerReceiver: connectListenerState.receiver,
                connectListenerCurrentTarget: connectListenerState.currentTarget
              }};
              port.onmessage = function (message) {{
                if (message.data === "ack") this.postMessage({{ phase: "complete", report, receiver: this === port, currentTarget: message.currentTarget === port }});
              }};
              port.postMessage({{ phase: "ready" }});
            }};
            "#
        );
        send_script(&mut entry, &body);
    });
    let import_server = thread::spawn(move || {
        let (mut stream, path) = accept_request(
            &import_listener,
            std::time::Instant::now() + Duration::from_secs(15),
        );
        assert_eq!(path, "/lib.js?lib=1");
        send_script(&mut stream, "globalThis.crossOriginImport = 'loaded';");
    });

    let mut runtime = Runtime::new();
    runtime.set_deadline(Duration::from_secs(10));
    let out = Captured::default();
    runtime.engine().ctx().op_state().put(ConsoleOut {
        out: Box::new(out.clone()),
        err: Box::new(Captured::default()),
    });
    let mut fetch = FetchConfig::default();
    fetch.set_route("wpt.test", worker_port, worker_address).expect("route shared worker fixture");
    fetch.set_route("alt.test", import_port, import_address).expect("route shared import fixture");
    fetch.set_require_routes(true);
    runtime.engine().ctx().op_state().put(fetch);

    let expected = format!(
        r#"{{"phase":"complete","report":{{"scope":true,"shared":true,"dedicatedInterfaceAbsent":true,"eventTarget":true,"workerLocation":true,"href":"http://wpt.test:{worker_port}/dir/entry.js?case=1","origin":"http://wpt.test:{worker_port}","type":"classic","name":"native-shared","imported":"loaded","initialNameIsInherited":true,"nameReplacementWorks":true,"nameRestored":true,"sourceMatchesPort":true,"eventTargetMatchesGlobal":true,"handlerThisIsGlobal":true,"trusted":true,"portsFrozen":true,"connectListenerCount":1,"connectListenerReceiver":true,"connectListenerCurrentTarget":true}},"receiver":true,"currentTarget":true}}"#
    );
    let source = format!(
        r#"
          globalThis.location = new URL("http://wpt.test:{worker_port}/tests/page.html?page=1");
          const worker = new SharedWorker("/redirect.js?case=1", {{ name: "native-shared" }});
          worker.port.onmessage = ({{ data }}) => {{
            if (data.phase === "ready") {{ worker.port.postMessage("ack"); return; }}
            console.log(JSON.stringify(data));
            worker.port.close();
          }};
          worker.onerror = ({{ message }}) => console.log("shared worker error", message);
        "#
    );
    match runtime.eval(&source).expect("HTTP SharedWorker source parses") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }
    assert!(!runtime.is_interrupted(), "shared-worker classic exchange did not settle");
    worker_server.join().expect("shared worker fixture exits");
    import_server.join().expect("shared import fixture exits");
    assert_eq!(out.lines(), [expected.as_str()]);
}

#[test]
fn shared_worker_http_redirect_never_requests_a_foreign_origin() {
    fn accept_request(listener: &TcpListener, deadline: std::time::Instant) -> (std::net::TcpStream, String) {
        let (mut stream, _) = loop {
            match listener.accept() {
                Ok(connection) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(std::time::Instant::now() < deadline, "timed out waiting for shared-worker redirect request");
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("accept shared-worker redirect request: {error}"),
            }
        };
        stream.set_nonblocking(false).expect("set shared-worker redirect blocking mode");
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .expect("set shared-worker redirect timeout");
        let mut request = Vec::new();
        let mut chunk = [0; 1024];
        loop {
            let count = stream.read(&mut chunk).expect("read shared-worker redirect request");
            if count == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..count]);
            if request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                break;
            }
        }
        let first_line = String::from_utf8_lossy(&request)
            .lines()
            .next()
            .expect("shared-worker redirect request line")
            .to_owned();
        let path = first_line
            .split_whitespace()
            .nth(1)
            .expect("shared-worker redirect request path")
            .to_owned();
        (stream, path)
    }

    let worker_listener = TcpListener::bind("127.0.0.1:0").expect("bind shared-worker redirect fixture");
    worker_listener.set_nonblocking(true).expect("set shared-worker redirect fixture nonblocking");
    let worker_address = worker_listener.local_addr().expect("shared-worker redirect address");
    let worker_port = worker_address.port();
    let foreign_listener = TcpListener::bind("127.0.0.1:0").expect("bind foreign worker fixture");
    foreign_listener.set_nonblocking(true).expect("set foreign worker fixture nonblocking");
    let foreign_address = foreign_listener.local_addr().expect("foreign worker address");
    let foreign_port = foreign_address.port();

    let worker_server = thread::spawn(move || {
        let (mut stream, path) = accept_request(
            &worker_listener,
            std::time::Instant::now() + Duration::from_secs(15),
        );
        assert_eq!(path, "/redirect.js");
        let location = format!("http://alt.test:{foreign_port}/entry.js");
        write!(
            stream,
            "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .expect("write cross-origin shared-worker redirect");
    });
    let foreign_probe = thread::spawn(move || {
        thread::sleep(Duration::from_millis(200));
        match foreign_listener.accept() {
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Ok(_) => panic!("same-origin worker loader issued a request to a foreign redirect target"),
            Err(error) => panic!("probe foreign worker listener: {error}"),
        }
    });

    let mut runtime = Runtime::new();
    runtime.set_deadline(Duration::from_secs(10));
    let out = Captured::default();
    runtime.engine().ctx().op_state().put(ConsoleOut {
        out: Box::new(out.clone()),
        err: Box::new(Captured::default()),
    });
    let mut fetch = FetchConfig::default();
    fetch.set_route("wpt.test", worker_port, worker_address).expect("route shared worker fixture");
    fetch.set_route("alt.test", foreign_port, foreign_address).expect("route foreign worker fixture");
    fetch.set_require_routes(true);
    runtime.engine().ctx().op_state().put(fetch);
    let source = format!(
        r#"
          globalThis.location = new URL("http://wpt.test:{worker_port}/tests/page.html");
          const worker = new SharedWorker("/redirect.js");
          worker.onerror = ({{ message }}) => {{
            console.log("foreign redirect denied", message.includes("same-origin policy"));
            worker.port.close();
          }};
        "#
    );
    match runtime.eval(&source).expect("HTTP SharedWorker source parses") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }
    assert!(!runtime.is_interrupted(), "shared-worker redirect error did not settle");
    worker_server.join().expect("shared-worker redirect server exits");
    foreign_probe.join().expect("foreign route remains unused");
    assert_eq!(out.lines(), ["foreign redirect denied true"]);
}

#[test]
fn dedicated_http_worker_uses_final_location_and_cross_origin_import_scripts() {
    fn accept_request(
        listener: &TcpListener,
        deadline: std::time::Instant,
        expected_path: &str,
    ) -> (std::net::TcpStream, String) {
        let (mut stream, _) = loop {
            match listener.accept() {
                Ok(connection) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "timed out waiting for worker HTTP request {expected_path}"
                    );
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("accept worker HTTP request: {error}"),
            }
        };
        // A stream accepted from the polling listener can inherit its
        // nonblocking mode on macOS; the existing read deadline requires a
        // blocking accepted stream, independently of listener polling.
        stream.set_nonblocking(false).expect("set worker request blocking mode");
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .expect("set worker request timeout");
        let mut request = Vec::new();
        let mut chunk = [0; 1024];
        loop {
            let count = stream.read(&mut chunk).expect("read worker HTTP request");
            if count == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..count]);
            if request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                break;
            }
        }
        let first_line = String::from_utf8_lossy(&request)
            .lines()
            .next()
            .expect("worker HTTP request line")
            .to_owned();
        let path = first_line
            .split_whitespace()
            .nth(1)
            .expect("worker HTTP request path")
            .to_owned();
        (stream, path)
    }

    fn send_script(stream: &mut std::net::TcpStream, mime: &str, body: &str) {
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .expect("write worker JavaScript response");
    }

    let worker_listener = TcpListener::bind("127.0.0.1:0").expect("bind worker fixture");
    worker_listener.set_nonblocking(true).expect("set worker fixture nonblocking");
    let worker_address = worker_listener.local_addr().expect("worker fixture address");
    let worker_port = worker_address.port();
    let import_listener = TcpListener::bind("127.0.0.1:0").expect("bind import fixture");
    import_listener.set_nonblocking(true).expect("set import fixture nonblocking");
    let import_address = import_listener.local_addr().expect("import fixture address");
    let import_port = import_address.port();

    let worker_server = thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let (mut redirect, path) =
            accept_request(&worker_listener, deadline, "/redirect.js?case=1");
        assert_eq!(path, "/redirect.js?case=1");
        redirect
            .write_all(b"HTTP/1.1 302 Found\r\nLocation: /dir/entry.js?case=1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .expect("write worker redirect");

        let (mut entry, path) =
            accept_request(&worker_listener, deadline, "/dir/entry.js?case=1");
        assert_eq!(path, "/dir/entry.js?case=1");
        let body = format!(
            r#"
            importScripts(
              "http://alt.test:{import_port}/lib.js?lib=1",
              "http://alt.test:{import_port}/state.js?state=1"
            );
            const importedLexicalsPersist = importedConst === 41 && importedLet === 43 &&
              importedLater === "initialized" && importedFunction() === 84;
            const importedLexicalsStayOffGlobal =
              !Object.prototype.hasOwnProperty.call(globalThis, "importedConst") &&
              !Object.prototype.hasOwnProperty.call(globalThis, "importedLet");
            const importedVarAndFunctionAreGlobal = globalThis.importedVar === 43 &&
              globalThis.importedFunction === importedFunction;
            let duplicateLexicalRejected = false;
            try {{ importScripts("http://alt.test:{import_port}/duplicate.js?case=1"); }}
            catch (error) {{ duplicateLexicalRejected = error instanceof SyntaxError; }}
            let syntaxErrorHasSourceUrl = false;
            try {{ importScripts("http://alt.test:{import_port}/invalid.js?case=1"); }}
            catch (error) {{
              syntaxErrorHasSourceUrl = error instanceof SyntaxError &&
                String(error.message).includes("http://alt.test:{import_port}/invalid.js?case=1");
            }}
            let thrownImportPreserved = false;
            let thrownImportHasSourceUrl = false;
            try {{ importScripts("http://alt.test:{import_port}/throw.js?case=1"); }}
            catch (error) {{
              thrownImportPreserved = error === globalThis.importThrownError &&
                error instanceof Error &&
                error.message === "classic-worker-import-failure";
              thrownImportHasSourceUrl = String(error.stack).includes(
                "http://alt.test:{import_port}/throw.js?case=1"
              );
            }}
            const originalSelf = self;
            self = 1;
            const selfAssignmentPreserved = self === originalSelf;
            const selfDescriptor = Object.getOwnPropertyDescriptor(globalThis, "self");
            const selfPropertyIsReadOnlyAccessor =
              selfDescriptor !== undefined &&
              typeof selfDescriptor.get === "function" &&
              selfDescriptor.set === undefined &&
              selfDescriptor.enumerable;
            let eventCount = 0;
            let eventReceiver = false;
            let eventCurrentTarget = false;
            const listener = function (event) {{
              eventCount++;
              eventReceiver = this === self;
              eventCurrentTarget = event.currentTarget === self;
            }};
            addEventListener("worker-probe", listener);
            const dispatchResult = dispatchEvent(new Event("worker-probe"));
            removeEventListener.call(null, "worker-probe", listener);
            const dispatchAfterRemove = dispatchEvent(new Event("worker-probe"));
            let invalidReceiverRejected = false;
            try {{ globalThis.addEventListener.call({{}}, "worker-probe", () => {{}}); }}
            catch (error) {{ invalidReceiverRejected = error instanceof TypeError && error.message.includes("EventTarget"); }}

            const receiveAck = function (event) {{
              if (event.data?.phase !== "ack") return;
              removeEventListener("message", receiveAck);
              postMessage({{
                phase: "report",
                scope: self instanceof WorkerGlobalScope,
                dedicated: self instanceof DedicatedWorkerGlobalScope,
                eventTarget: self instanceof EventTarget,
                workerLocation: location instanceof WorkerLocation,
                href: location.href,
                type,
                name,
                imported: globalThis.crossOriginImport,
                selfAssignmentPreserved,
                selfPropertyIsReadOnlyAccessor,
                eventCount,
                eventReceiver,
                eventCurrentTarget,
                dispatchResult,
                dispatchAfterRemove,
                invalidReceiverRejected,
                messageReceiver: this === self && event.currentTarget === self,
                messageInterface: event instanceof MessageEvent,
                nodeGlobalsAbsent: typeof process === "undefined" &&
                  typeof Buffer === "undefined" && typeof require === "undefined",
                importedLexicalsPersist,
                importedLexicalsStayOffGlobal,
                importedVarAndFunctionAreGlobal,
                importedTdzObserved,
                importedState: globalThis.importScriptState,
                duplicateLexicalRejected,
                duplicateScriptBodyDidNotRun: globalThis.duplicateScriptBodyRan === undefined,
                syntaxErrorHasSourceUrl,
                thrownImportPreserved,
                thrownImportHasSourceUrl,
              }});
            }};
            addEventListener("message", receiveAck);
            postMessage({{ phase: "ready" }});
            "#
        );
        send_script(&mut entry, "text/javascript; charset=utf-8", &body);
    });
    let import_server = thread::spawn(move || {
        let scripts = [
            (
                "/lib.js?lib=1",
                "const importedConst = 41;\nlet importedLet = 42;\nvar importedVar = 43;\nfunction importedFunction() { return importedConst + importedLet; }\nlet importedTdzObserved = false;\ntry { importedLater; } catch (error) { importedTdzObserved = error instanceof ReferenceError; }\nlet importedLater = 'initialized';\nglobalThis.crossOriginImport = 'loaded';",
            ),
            (
                "/state.js?state=1",
                "importedLet++;\nglobalThis.importScriptState = { constValue: importedConst, letValue: importedLet, varValue: importedVar, functionValue: importedFunction(), tdzObserved: importedTdzObserved, laterValue: importedLater };",
            ),
            (
                "/duplicate.js?case=1",
                "globalThis.duplicateScriptBodyRan = true; const importedConst = 99;",
            ),
            (
                "/invalid.js?case=1",
                "const = ;",
            ),
            (
                "/throw.js?case=1",
                "globalThis.importThrownError = new Error('classic-worker-import-failure'); throw globalThis.importThrownError;",
            ),
        ];
        for (expected_path, body) in scripts {
            let (mut stream, path) = accept_request(
                &import_listener,
                std::time::Instant::now() + Duration::from_secs(15),
                expected_path,
            );
            assert_eq!(path, expected_path);
            send_script(&mut stream, "application/javascript", body);
        }
    });

    let mut runtime = Runtime::new();
    let out = Captured::default();
    runtime.engine().ctx().op_state().put(ConsoleOut {
        out: Box::new(out.clone()),
        err: Box::new(Captured::default()),
    });
    let mut fetch = FetchConfig::default();
    fetch
        .set_route("wpt.test", worker_port, worker_address)
        .expect("route worker fixture");
    fetch
        .set_route("alt.test", import_port, import_address)
        .expect("route import fixture");
    fetch.set_require_routes(true);
    runtime.engine().ctx().op_state().put(fetch);

    let expected = format!(
        r#"{{"phase":"report","scope":true,"dedicated":true,"eventTarget":true,"workerLocation":true,"href":"http://wpt.test:{worker_port}/dir/entry.js?case=1","type":"classic","name":"native-wpt","imported":"loaded","selfAssignmentPreserved":true,"selfPropertyIsReadOnlyAccessor":true,"eventCount":1,"eventReceiver":true,"eventCurrentTarget":true,"dispatchResult":true,"dispatchAfterRemove":true,"invalidReceiverRejected":true,"messageReceiver":true,"messageInterface":true,"nodeGlobalsAbsent":true,"importedLexicalsPersist":true,"importedLexicalsStayOffGlobal":true,"importedVarAndFunctionAreGlobal":true,"importedTdzObserved":true,"importedState":{{"constValue":41,"letValue":43,"varValue":43,"functionValue":84,"tdzObserved":true,"laterValue":"initialized"}},"duplicateLexicalRejected":true,"duplicateScriptBodyDidNotRun":true,"syntaxErrorHasSourceUrl":true,"thrownImportPreserved":true,"thrownImportHasSourceUrl":true}}"#
    );
    let source = format!(
        r#"
          globalThis.location = new URL("http://wpt.test:{worker_port}/tests/page.html?page=1");
          try {{ new Worker("http://wrong.test:{worker_port}/worker.js"); }}
          catch (error) {{ console.log("origin", error.name); }}
          const worker = new Worker("/redirect.js?case=1", {{ name: "native-wpt" }});
          worker.onmessage = ({{ data }}) => {{
            if (data.phase === "ready") {{
              worker.postMessage({{ phase: "ack" }});
              return;
            }}
            console.log(JSON.stringify(data));
            worker.terminate();
          }};
          worker.onerror = ({{ message }}) => {{
            console.log("worker error", message);
            worker.terminate();
          }};
        "#
    );
    match runtime.eval(&source).expect("dedicated worker source parses") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }
    worker_server.join().expect("worker fixture exits");
    assert!(
        import_server.join().is_ok(),
        "import fixture thread panicked; worker console: {:?}",
        out.lines()
    );
    assert_eq!(out.lines(), ["origin SecurityError", expected.as_str()]);
}
