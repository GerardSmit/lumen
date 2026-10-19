//! Message-local native capabilities survive reentrant serialization and reject forged indexes.
use lumen_runtime::{Completion, Runtime};

fn check(source: &str) {
    let mut runtime = Runtime::new();
    match runtime.eval(source).expect("source parses") {
        Completion::Value(value) => assert_eq!(value, "passed"),
        Completion::Throw { name, message } => panic!("{name}: {message}"),
    }
}

#[test]
fn nested_message_from_getter_preserves_outer_capability_frame() {
    check(
        r#"
        const assert = require('node:assert/strict');
        const { MessageChannel, receiveMessageOnPort } = require('node:worker_threads');
        const { port1, port2 } = new MessageChannel();
        const outer = new SharedArrayBuffer(4), inner = new SharedArrayBuffer(4);
        new Int32Array(outer)[0] = 11;
        new Int32Array(inner)[0] = 22;
        port2.postMessage({ before: outer, get nested() { port2.postMessage({ inner }); return 7; }, after: outer });
        const first = receiveMessageOnPort(port1).message;
        const second = receiveMessageOnPort(port1).message;
        assert.equal(new Int32Array(first.inner)[0], 22);
        assert.equal(second.before, second.after);
        assert.equal(second.nested, 7);
        new Int32Array(second.before)[0] = 33;
        assert.equal(new Int32Array(outer)[0], 33);
        assert.equal(new Int32Array(inner)[0], 22);
        port1.close(); port2.close();
        'passed'
    "#,
    );
}

#[test]
fn failed_clones_release_staging_and_keep_sender_ports_owned() {
    check(
        r#"
        const assert = require('node:assert/strict');
        const { MessageChannel, receiveMessageOnPort } = require('node:worker_threads');
        const channel = new MessageChannel(), transferable = new MessageChannel();
        for (let attempt = 0; attempt < 100; attempt++) {
            assert.throws(() => channel.port2.postMessage({ port: transferable.port2, bad() {} }, [transferable.port2]));
        }
        transferable.port2.postMessage('still owned');
        assert.equal(receiveMessageOnPort(transferable.port1).message, 'still owned');
        channel.port2.postMessage(new SharedArrayBuffer(4));
        assert(receiveMessageOnPort(channel.port1).message instanceof SharedArrayBuffer);
        channel.port1.close(); channel.port2.close();
        transferable.port1.close(); transferable.port2.close();
        'passed'
    "#,
    );
}

#[test]
fn forged_capability_indexes_and_excess_attachments_are_refused() {
    check(
        r#"
        const assert = require('node:assert/strict');
        const { MessageChannel, receiveMessageOnPort } = require('node:worker_threads');
        for (const index of [-1, 0, 1.5, NaN, Infinity, 1024]) {
            assert.throws(() => __cloneTransfer.importShared(index));
        }
        const { port1, port2 } = new MessageChannel();
        const many = Array.from({length: 1025}, () => new SharedArrayBuffer(0));
        assert.throws(() => port2.postMessage(many));
        port2.postMessage(new SharedArrayBuffer(4));
        assert(receiveMessageOnPort(port1).message instanceof SharedArrayBuffer);
        assert.throws(() => __cloneTransfer.importShared(0), 'Completed delivery must retire its capability frame');
        port2.postMessage('recovered');
        assert.equal(receiveMessageOnPort(port1).message, 'recovered');
        assert.equal(receiveMessageOnPort(port1), undefined);
        port1.close(); port2.close();
        'passed'
    "#,
    );
}

#[test]
fn native_buffer_detachment_requires_genuine_attached_ordinary_backing() {
    check(
        r#"
        const assert = require('node:assert/strict');
        const fake = Object.create(ArrayBuffer.prototype);
        const shared = new SharedArrayBuffer(4);
        const resizable = new ArrayBuffer(4, {maxByteLength: 8});
        for (const value of [fake, shared, resizable, new Uint8Array(4), {}]) {
            assert.equal(__cloneTransfer.isTransferableBuffer(value), false);
            assert.throws(() => __cloneTransfer.detachBuffer(value));
        }
        const buffer = new ArrayBuffer(4), view = new Uint8Array(buffer);
        view[0] = 42;
        assert.equal(__cloneTransfer.isTransferableBuffer(buffer), true);
        __cloneTransfer.detachBuffer(buffer);
        assert.equal(buffer.byteLength, 0);
        assert.equal(view.byteLength, 0);
        assert.equal(__cloneTransfer.isTransferableBuffer(buffer), false);
        assert.throws(() => __cloneTransfer.detachBuffer(buffer));
        assert.equal(shared.byteLength, 4);
        assert.equal(resizable.byteLength, 4);
        assert.equal(resizable.maxByteLength, 8);
        'passed'
    "#,
    );
}

#[test]
fn local_port_clone_validates_before_getters_and_commits_only_after_success() {
    check(
        r#"
        const assert=require('node:assert/strict');
        const {MessageChannel,MessagePort,receiveMessageOnPort}=require('node:worker_threads');
        const channel=new MessageChannel();
        const sentinel=new Error('live sentinel');
        let reads=0;
        const validation={get value(){reads++;throw sentinel;}};
        assert.throws(()=>structuredClone(validation,{transfer:[channel.port2]}), error=>error===sentinel);
        assert.equal(reads,1);
        channel.port2.postMessage('still live');
        assert.equal(receiveMessageOnPort(channel.port1).message,'still live');
        const moved=structuredClone({port:channel.port2,again:channel.port2},{transfer:[channel.port2]});
        assert(moved.port instanceof MessagePort);
        assert.equal(moved.port,moved.again);
        moved.port.postMessage('moved');
        assert.equal(receiveMessageOnPort(channel.port1).message,'moved');
        assert.throws(()=>structuredClone(validation,{transfer:[channel.port2]}),error=>error.name==='DataCloneError');
        assert.equal(reads,1);
        moved.port.close();
        assert.throws(()=>structuredClone(validation,{transfer:[channel.port1]}),error=>error.name==='DataCloneError');
        assert.equal(reads,1);
        channel.port1.close();
        'passed'
    "#,
    );
}

#[test]
fn local_shared_clone_preserves_backing_and_rejects_unlisted_ports() {
    check(
        r#"
        const assert=require('node:assert/strict');
        const shared=new SharedArrayBuffer(8);
        const input={first:shared,second:shared,view:new Int32Array(shared)};
        const copied=structuredClone(input);
        assert(copied.first instanceof SharedArrayBuffer);
        assert.notEqual(copied.first,shared);
        assert.equal(copied.first,copied.second);
        assert.equal(copied.view.buffer,copied.first);
        copied.view[1]=42;
        assert.equal(new Int32Array(shared)[1],42);
        const {MessageChannel}=require('node:worker_threads');
        const channel=new MessageChannel();
        assert.throws(()=>structuredClone({port:channel.port1}), error=>error.name==='DataCloneError');
        channel.port1.close();channel.port2.close();
        'passed'
    "#,
    );
}

#[test]
fn node_ports_carry_the_native_clone_graph_semantics() {
    check(
        r#"
        const assert=require('node:assert/strict');
        const {MessageChannel,receiveMessageOnPort}=require('node:worker_threads');
        const {port1,port2}=new MessageChannel();
        const date=new Date(3);
        const map=new Map();
        map.set('map',map);
        const list=Object.assign([1,,3],{tag:'t'});
        port2.postMessage({a:date,b:date,map,list,error:new Error('x',{cause:'why'})});
        const got=receiveMessageOnPort(port1).message;
        assert.equal(got.a,got.b);
        assert.equal(got.map.get('map'),got.map);
        assert.equal(got.list.length,3);
        assert.equal(1 in got.list,false);
        assert.equal(got.list.tag,'t');
        assert.equal(got.error.cause,'why');
        assert.throws(()=>port2.postMessage({proxy:new Proxy({},{})}),error=>error.name==='DataCloneError');
        port1.close(); port2.close();
        'passed'
    "#,
    );
}

#[test]
fn unlisted_and_uncloneable_values_leave_no_outgoing_frame() {
    check(
        r#"
        const assert=require('node:assert/strict');
        const {MessageChannel,receiveMessageOnPort}=require('node:worker_threads');
        const {port1,port2}=new MessageChannel();
        const shared=new SharedArrayBuffer(4);
        for (let attempt=0;attempt<100;attempt++) {
            assert.throws(()=>port2.postMessage({shared,weak:new WeakMap()}),error=>error.name==='DataCloneError');
        }
        port2.postMessage(shared);
        assert(receiveMessageOnPort(port1).message instanceof SharedArrayBuffer);
        port1.close(); port2.close();
        'passed'
    "#,
    );
}
