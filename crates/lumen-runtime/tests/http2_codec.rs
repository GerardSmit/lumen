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

#[test]
fn frames_and_hpack_round_trip_and_reject_invalid_input() {
    let mut runtime = Runtime::new();
    let out = Captured::default();
    runtime.engine().ctx().op_state().put(ConsoleOut {
        out: Box::new(out.clone()),
        err: Box::new(Captured::default()),
    });
    let source = r#"
        const codec = globalThis.__lumenHttp2Codec;
        const encoder = new codec.Encoder(4096), decoder = new codec.Decoder(4096);
        const list = [[":method", "GET", false], [":path", "/", false],
                      ["content-type", "text/plain", false], ["x-test", "yes", false],
                      ["authorization", "secret", false]];
        const first = encoder.encode(list);
        const again = encoder.encode(list);
        const show = (headers) => headers.map(([n, v, never]) => `${n}=${v}${never ? "!" : ""}`).join(";");
        console.log("headers", show(decoder.decode(first)));
        console.log("indexed", again.length < first.length, show(decoder.decode(again)) === show(list.map(([n, v]) => [n, v, n === "authorization"])));

        const huffman = globalThis.__lumenHpackHuffman;
        const encodedText = huffman.encode(Buffer.from("www.example.com"));
        console.log("huffman", encodedText.toString("hex"), huffman.decode(encodedText).toString());
        const everyByte = Buffer.alloc(256);
        for (let i = 0; i < everyByte.length; i++) everyByte[i] = i;
        console.log("huffman-bytes", huffman.decode(huffman.encode(everyByte)).equals(everyByte));

        try { new codec.Decoder().decode(Buffer.from([0x80])); }
        catch (error) { console.log("index-error", error.code); }
        try { new codec.Decoder().decode(Buffer.from([0x40, 0x7f])); }
        catch (error) { console.log("truncated-error", error.code); }
        try { huffman.decode(Buffer.from([0xff])); }
        catch (error) { console.log("hpack-error", error.code); }
    "#;
    match runtime.eval(source).expect("source parses") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }
    let lines: Vec<_> = String::from_utf8(out.0.borrow().clone())
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(
        lines,
        [
            "headers :method=GET;:path=/;content-type=text/plain;x-test=yes;authorization=secret!",
            "indexed true true",
            "huffman f1e3c2e5f23a6ba0ab90f4ff www.example.com",
            "huffman-bytes true",
            "index-error ERR_HTTP2_COMPRESSION_ERROR",
            "truncated-error ERR_HTTP2_COMPRESSION_ERROR",
            "hpack-error ERR_HTTP2_COMPRESSION_ERROR",
        ]
    );
}
