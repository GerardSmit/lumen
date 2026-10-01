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
    let mut runtime=Runtime::new();
    let out=Captured::default();
    runtime.engine().ctx().op_state().put(ConsoleOut {out:Box::new(out.clone()),err:Box::new(Captured::default())});
    runtime.eval(r#"
        const {MessageChannel}=require('node:worker_threads');
        const {port1,port2}=new MessageChannel();
        port1.on('message',value=>console.log('message',value));
        port1.on('close',()=>console.log('close'));
        port2.postMessage('receipt');
        port2.close();
    "#).expect("parse");
    assert_eq!(out.lines(),["message receipt","close"]);
}
