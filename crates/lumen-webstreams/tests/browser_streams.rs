use lumen_host::Completion;
use lumen_runtime::Runtime;

fn runtime() -> Runtime {
    let mut runtime = Runtime::new();
    lumen_host::install(runtime.engine(), &[lumen_webstreams::extension()]);
    runtime
}

fn eval(runtime: &mut Runtime, source: &str) {
    match runtime.eval(source).expect("script parses") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("{name}: {message}"),
    }
}

#[test]
fn installs_browser_stream_classes_without_node_compression() {
    let mut engine = runtime();
    eval(
        &mut engine,
        r#"
        if (typeof ReadableStream !== 'function' ||
            typeof ReadableStreamBYOBReader !== 'function' ||
            typeof WritableStream !== 'function' ||
            typeof TransformStream !== 'function' ||
            typeof ByteLengthQueuingStrategy !== 'function' ||
            typeof TextEncoderStream !== 'function' ||
            typeof TextDecoderStream !== 'function') {
          throw new Error('wrong browser streams surface');
        }
        "#,
    );
}

#[test]
fn browser_artifact_does_not_register_node_compression_adapters() {
    let source = lumen_webstreams::source();
    assert!(!source.contains("defineModule(\"internal/webstreams/compression\""));
    assert!(!source.contains("Object.defineProperty(globalThis, \"CompressionStream\""));
    assert!(!source.contains("Object.defineProperty(globalThis, \"DecompressionStream\""));
}

#[test]
fn browser_byte_byob_tee_and_cancel_use_generated_stream_algorithms() {
    let mut engine = runtime();
    eval(
        &mut engine,
        r#"
        var streamResult = false;
        (async () => {
          let cancelled = false;
          const source = new ReadableStream({
            type: 'bytes',
            start(controller) { controller.enqueue(new Uint8Array([7, 9])); controller.close(); },
          });
          const byob = source.getReader({ mode: 'byob' });
          const part = await byob.read(new Uint8Array(8));
          const [left, right] = new ReadableStream({
            start(controller) { controller.enqueue('shared'); controller.close(); },
          }).tee();
          const l = await left.getReader().read();
          const r = await right.getReader().read();
          await new ReadableStream({ cancel() { cancelled = true; } }).cancel();
          streamResult = [part.done, part.value.length, part.value[0], l.value, r.value, cancelled].join('|');
        })().catch((error) => { throw error; });
        "#,
    );
    eval(&mut engine, "if (streamResult!=='false|2|7|shared|shared|true') throw new Error('byte stream: '+streamResult)");
}

#[test]
fn browser_byte_stream_clone_tee_preserves_both_chunks() {
    let mut engine = runtime();
    eval(
        &mut engine,
        r#"
        var byteTeeResult = 'pending';
        (async () => {
          let pulled = false;
          const source = new ReadableStream({
            type: 'bytes',
            async pull(controller) {
              if (pulled) return;
              pulled = true;
              await Promise.resolve();
              controller.enqueue(new Uint8Array([4, 8]));
              controller.close();
            },
          });
          const [left, right] = source[Symbol.for('lumen.cloneBody')]();
          const [a, b] = await Promise.all([
            left.getReader().read(),
            right.getReader().read(),
          ]);
          byteTeeResult = [a.value.join(','), b.value.join(','), a.done, b.done].join('|');
        })().catch(error => { byteTeeResult = error.name + ':' + error.message + ':' + error.stack; });
        "#,
    );
    eval(
        &mut engine,
        "if (byteTeeResult!=='4,8|4,8|false|false') throw new Error('byte tee clone: '+byteTeeResult)",
    );
}

#[test]
fn browser_fetch_response_clone_tees_native_byte_reader_chunks() {
    let mut engine = runtime();
    eval(
        &mut engine,
        r#"
        globalThis.URL = class { constructor(value) { this.href = String(value); } };
        globalThis.Blob = class {};
        globalThis.FormData = class {};
        globalThis.URLSearchParams = class {};
        let nativeRead = 0;
        globalThis.__bitnestHttp = {
          now: () => 0,
          timeOrigin: 0,
          request(_method, url, _headers, _body, resolve) {
            resolve({
              status: 200,
              statusText: 'OK',
              url,
              headers: [['content-type', 'application/octet-stream']],
              bodyReader: {
                read() {
                  nativeRead++;
                  return Promise.resolve(nativeRead === 1 ? new Uint8Array([4, 8]) : null);
                },
                cancel() {},
              },
            });
            return { abort() {} };
          },
        };
        "#,
    );
    let fetch_source = include_str!("../../lumen-web/src/js/fetch.js");
    eval(
        &mut engine,
        &format!("(() => {{ const __http=globalThis.__bitnestHttp; {fetch_source} }})()"),
    );
    eval(
        &mut engine,
        r#"
        var fetchCloneResult = 'pending';
        fetch('http://example.test/data').then(response => {
          const clone = response.clone();
          return Promise.all([response.bytes(), clone.bytes()]);
        }).then(([a, b]) => {
          fetchCloneResult = a.join(',') + '|' + b.join(',');
        }).catch(error => {
          fetchCloneResult = error.name + ':' + error.message + ':' + error.stack;
        });
        "#,
    );
    eval(
        &mut engine,
        "if (fetchCloneResult!=='4,8|4,8') throw new Error('Fetch byte clone: '+fetchCloneResult)",
    );
}

#[test]
fn browser_writable_transform_obeys_read_demand_and_closes_in_order() {
    let mut engine = runtime();
    eval(
        &mut engine,
        r#"
        var transformResult = false;
        (async () => {
          const transform = new TransformStream({
            transform(value, controller) { controller.enqueue(value * 2); },
          });
          const reader = transform.readable.getReader();
          const writer = transform.writable.getWriter();
          let writeFinished = false;
          const writing = writer.write(21).then(() => { writeFinished = true; });
          const writeWasBackpressured = !writeFinished;
          const first = await reader.read();
          await writing;
          await writer.close();
          const end = await reader.read();
          transformResult = [writeWasBackpressured, first.value, first.done, end.done].join('|');
        })().catch((error) => { throw error; });
        "#,
    );
    eval(&mut engine, "if (transformResult!=='true|42|false|true') throw new Error('transform: '+transformResult)");
}

#[test]
fn browser_text_streams_pipe_chunks_between_the_host_encoding_classes() {
    let mut engine = runtime();
    eval(
        &mut engine,
        r#"
        var textStreamResult = false;
        (async () => {
          const encoder = new TextEncoderStream();
          const byteReader = encoder.readable.getReader();
          const textWriter = encoder.writable.getWriter();
          const writing = textWriter.write('stream');
          const bytes = await byteReader.read();
          await writing;
          await textWriter.close();

          const decoder = new TextDecoderStream();
          const textReader = decoder.readable.getReader();
          const byteWriter = decoder.writable.getWriter();
          const decoding = byteWriter.write(bytes.value);
          const text = await textReader.read();
          await decoding;
          await byteWriter.close();
          textStreamResult = [text.value, text.done, bytes.value.length].join('|');
        })().catch((error) => { throw error; });
        "#,
    );
    eval(&mut engine, "if (textStreamResult!=='stream|false|6') throw new Error('text stream: '+textStreamResult)");
}
