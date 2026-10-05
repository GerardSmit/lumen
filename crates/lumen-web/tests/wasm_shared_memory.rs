//! Guard WebAssembly shared Memory's SharedArrayBuffer identity and growth contract.

fn eval_shared_memory_contract(body: &'static str) {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            let mut engine = lumen::Engine::new();
            lumen_host::install(&mut engine, &[lumen_web::extension()]);
            match engine
                .eval(body, false)
                .expect("parse shared WebAssembly.Memory contract")
            {
                lumen::Completion::Value(value) => assert_eq!(value, "true"),
                lumen::Completion::Throw { name, message } => panic!("{name}: {message}"),
            }
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn shared_memory_keeps_old_buffer_length_and_returns_aliased_new_views() {
    eval_shared_memory_contract(
        r#"
            const memory = new WebAssembly.Memory({initial:1, maximum:2, shared:true});
            const first = memory.buffer;
            const firstView = new Int32Array(first);
            firstView[0] = 17;
            const zeroGrow = memory.grow(0);
            const afterZeroGrow = memory.buffer;
            const oldPages = memory.grow(1);
            const second = memory.buffer;
            const secondView = new Int32Array(second);
            secondView[1] = 29;
            const aliases = firstView[0] === 17 && secondView[0] === 17 && firstView[1] === 29;
            const failedGrow = (() => { try { memory.grow(1); } catch (error) {
                return error instanceof RangeError;
            } return false; })();
            zeroGrow === 1 && afterZeroGrow !== first && oldPages === 1 &&
                first instanceof SharedArrayBuffer && second instanceof SharedArrayBuffer &&
                first !== second && afterZeroGrow !== second && first.byteLength === 65536 &&
                afterZeroGrow.byteLength === 65536 && second.byteLength === 131072 &&
                memory.buffer === second && aliases && failedGrow && memory.buffer === second;
        "#,
    );
}

#[test]
fn shared_memory_requires_bounded_valid_descriptors() {
    eval_shared_memory_contract(
        r#"
            const rejects = (descriptor, name) => {
                try { new WebAssembly.Memory(descriptor); }
                catch (error) { return error.name === name; }
                return false;
            };
            rejects({initial:0, shared:true}, 'TypeError') &&
                rejects({maximum:1, shared:true}, 'TypeError') &&
                rejects({initial:2, maximum:1, shared:true}, 'RangeError') &&
                rejects({initial:0, maximum:4097, shared:true}, 'RangeError') &&
                rejects({initial:undefined}, 'TypeError') &&
                [NaN, Infinity, -Infinity, -1, 0x100000000].every(value =>
                    rejects({initial:value}, 'TypeError') &&
                    rejects({initial:0, maximum:value}, 'TypeError')) &&
                (() => {
                    const order = [];
                    const ordered = new WebAssembly.Memory({
                        get initial() {
                            order.push('initial');
                            return {valueOf() { order.push('initial valueOf'); return 1; }};
                        },
                        get maximum() {
                            order.push('maximum');
                            return {valueOf() { order.push('maximum valueOf'); return 1.5; }};
                        },
                        get shared() { order.push('shared'); return {}; }
                    });
                    return order.join(',') === 'initial,initial valueOf,maximum,maximum valueOf,shared' &&
                        ordered.buffer instanceof SharedArrayBuffer && ordered.grow(0) === 1 &&
                        ordered.buffer.byteLength === 65536;
                })();
        "#,
    );
}

#[test]
fn imported_shared_memory_locks_across_wasm_and_host_callback_access() {
    eval_shared_memory_contract(
        r#"
            const section = (id, payload) => [id, payload.length, ...payload];
            const name = value => [value.length, ...Array.from(value, c => c.charCodeAt(0))];
            const functionBody = [
                0,
                0x41, 0, 0x41, 17, 0x36, 2, 0,
                0x10, 0,
                0x41, 1, 0x40, 0, 0x1a,
                0x41, 0, 0x28, 2, 0,
                0x0b
            ];
            const bytes = new Uint8Array([
                0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00,
                ...section(1, [2, 0x60, 0, 0, 0x60, 0, 1, 0x7f]),
                ...section(2, [
                    2,
                    ...name('env'), ...name('memory'), 2, 3, 1, 2,
                    ...name('env'), ...name('callback'), 0, 0
                ]),
                ...section(3, [1, 1]),
                ...section(7, [1, ...name('run'), 0, 1]),
                ...section(10, [1, functionBody.length, ...functionBody])
            ]);
            const memory = new WebAssembly.Memory({initial:1, maximum:2, shared:true});
            const oldBuffer = memory.buffer;
            let callbackSawStore = false;
            const instance = new WebAssembly.Instance(new WebAssembly.Module(bytes), {
                env: {
                    memory,
                    callback() {
                        const view = new Int32Array(memory.buffer);
                        callbackSawStore = memory.buffer === oldBuffer && view[0] === 17;
                        view[0] = 23;
                    }
                }
            });
            const result = instance.exports.run();
            const grownBuffer = memory.buffer;
            callbackSawStore && result === 23 && grownBuffer instanceof SharedArrayBuffer &&
                grownBuffer !== oldBuffer && oldBuffer.byteLength === 65536 &&
                grownBuffer.byteLength === 131072 && new Int32Array(oldBuffer)[0] === 23 &&
                new Int32Array(grownBuffer)[0] === 23;
        "#,
    );
}

#[test]
fn shared_buffers_route_all_views_codecs_and_growth_through_the_shared_backing() {
    eval_shared_memory_contract(
        r#"
            const buffer = new SharedArrayBuffer(8, {maxByteLength:16});
            const bytes = new Uint8Array(buffer);
            bytes.set([1, 2, 3, 4]);
            const sub = bytes.subarray(1, 4);
            sub[0] = 5;
            const data = new DataView(buffer);
            data.setUint8(3, 7);
            const typedCopy = bytes.slice(1, 4);
            typedCopy[0] = 99;
            const sharedCopy = buffer.slice(0, 4);
            const isolatedCopies = typedCopy[0] === 99 &&
                Array.from(new Uint8Array(sharedCopy)).join(',') === '1,5,3,7';
            const encoded = new TextEncoder().encodeInto('ok', new Uint8Array(buffer, 4, 4));
            const decoded = new TextDecoder().decode(new Uint8Array(buffer, 4, 2));
            const codecs = encoded.read === 2 && encoded.written === 2 && decoded === 'ok' &&
                Array.from(new Uint8Array(buffer, 0, 4)).join(',') === '1,5,3,7';

            const wideBuffer = new SharedArrayBuffer(32);
            const wideBytes = new Uint8Array(wideBuffer);
            const wide = new DataView(wideBuffer);
            wide.setInt8(0, -2);
            wide.setUint16(1, 0x1234, true);
            wide.setInt32(3, -123456, false);
            wide.setBigUint64(7, 0x0102030405060708n, true);
            wide.setFloat32(15, 1.5, false);
            wide.setFloat64(19, -2.25, true);
            wide.setUint32(27, 0x01020304, false);
            const endianAndSizes = wide.getInt8(0) === -2 &&
                wide.getUint16(1, true) === 0x1234 && wideBytes[1] === 0x34 && wideBytes[2] === 0x12 &&
                wide.getInt32(3, false) === -123456 &&
                wide.getBigUint64(7, true) === 0x0102030405060708n &&
                wide.getFloat32(15, false) === 1.5 && wide.getFloat64(19, true) === -2.25 &&
                wide.getUint32(27, false) === 0x01020304 &&
                (() => { try { wide.getUint16(31); } catch (error) {
                    return error instanceof RangeError;
                } return false; })();

            const growable = new SharedArrayBuffer(4, {maxByteLength:8});
            const fixed = new Uint8Array(growable, 0, 4);
            const tracking = new Uint8Array(growable);
            const trackingView = new DataView(growable);
            fixed[0] = 11;
            growable.grow(8);
            tracking[7] = 77;
            trackingView.setUint8(6, 66);
            const growth = growable.byteLength === 8 && fixed.length === 4 &&
                tracking.length === 8 && trackingView.byteLength === 8 &&
                new Uint8Array(growable)[0] === 11 && new Uint8Array(growable)[6] === 66 &&
                new Uint8Array(growable)[7] === 77;
            isolatedCopies && codecs && endianAndSizes && growth && data.getUint8(3) === 7;
        "#,
    );
}
