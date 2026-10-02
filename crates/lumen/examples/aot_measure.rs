//! Initial milestone-0 baseline: AOT-BC section sizes and exercised JIT code.
//! Run with LUMEN_JIT_EAGER=1; counts are generated regions, not complete AOT functions.
use lumen::precompiled::{CompiledUnit, PrecompileBundle, SEC_MANIFEST};
use lumen::{Completion, Engine, SourceKind};
use lumen_common::{aot, lzh};
use std::time::Instant;

fn main() -> Result<(), String> {
    println!("program,source_bytes,functions,chunks,refused,blob_bytes,raw_blob_bytes,bytecode_store_bytes,bytecode_lzh_bytes,lzh_decode_mbps,jit_units,jit_bytes,native_entries,eval_ms");
    for path in std::env::args().skip(1) {
        let src = std::fs::read_to_string(&path).map_err(|e| format!("{path}: {e}"))?;
        let unit = CompiledUnit::compile(&src, SourceKind::Script)?;
        let stats = unit.bytecode_stats();
        let mut bundle = PrecompileBundle::new();
        bundle.add_compiled("app.js", unit)?;
        let blob = bundle.finish();
        let mut raw = PrecompileBundle::new();
        raw.set_compress_source(false);
        raw.add("app.js", &src, SourceKind::Script)?;
        let raw_len = raw.finish().len();
        let container = aot::Container::parse(&blob)?;
        let manifest = container
            .sections
            .iter()
            .find(|s| s.kind == SEC_MANIFEST)
            .unwrap()
            .data;
        let mut pos = 0;
        aot::read_varint(manifest, &mut pos)?; // body-store section
        let bc_index = aot::read_varint(manifest, &mut pos)? as usize;
        let store = if bc_index == 0 {
            &[][..]
        } else {
            container.sections[bc_index].data
        };
        let compressed = lzh::compress(store);
        let start = Instant::now();
        let mut iterations = 0;
        while start.elapsed().as_millis() < 100 {
            assert_eq!(lzh::decompress_bounded(&compressed, store.len())?, store);
            iterations += 1;
        }
        let mbps =
            store.len() as f64 * iterations as f64 / start.elapsed().as_secs_f64() / 1_000_000.0;
        let mut engine = Engine::new();
        engine.set_tier_threshold(0);
        let start = Instant::now();
        match engine.eval(&src, false).map_err(|e| e.message)? {
            Completion::Value(_) => {}
            Completion::Throw { name, message } => {
                return Err(format!("{path}: {name}: {message}"))
            }
        }
        let ms = start.elapsed().as_secs_f64() * 1000.0;
        let jit = engine.jit_stats();
        println!(
            "\"{}\",{},{},{},{},{},{},{},{},{:.2},{},{},{},{:.2}",
            path.replace('"', "\"\""),
            src.len(),
            stats.functions,
            stats.chunks,
            stats.refused,
            blob.len(),
            raw_len,
            store.len(),
            compressed.len(),
            mbps,
            jit.compiled_units,
            jit.code_bytes,
            jit.executed_entries,
            ms
        );
    }
    Ok(())
}
