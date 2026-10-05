//! Host-only codec experiment; compressed bytes are not a runnable native container.
use std::{
    hint::black_box,
    time::{Duration, Instant},
};

fn measure(program: &str, region: &str, raw: &[u8]) -> Result<(), String> {
    let frame = lumen_common::compress::deflate_best(raw);
    if lumen_common::compress::deflate_best(raw) != frame {
        return Err("non-deterministic deflate stream".into());
    }
    if lumen_common::compress::inflate_limited(&frame, raw.len())? != raw {
        return Err("deflate round trip mismatch".into());
    }
    // Include decoder allocation/free cost, but exclude disk IO and compression.
    let mut samples = Vec::new();
    for _ in 0..5 {
        let start = Instant::now();
        let mut iterations = 0u64;
        loop {
            black_box(lumen_common::compress::inflate_limited(
                black_box(&frame),
                raw.len(),
            )?);
            iterations += 1;
            if start.elapsed() >= Duration::from_millis(100) {
                break;
            }
        }
        samples.push(start.elapsed().as_secs_f64() / iterations as f64);
    }
    samples.sort_by(f64::total_cmp);
    let seconds = samples[2];
    println!(
        "\"{}\",{region},{},{},{:.3},{:.3},{:.3},{:.3}",
        program.replace('"', "\"\""),
        raw.len(),
        frame.len(),
        seconds * 1e6,
        raw.len() as f64 / seconds / 1048576.0,
        samples[0] * 1e6,
        samples[4] * 1e6
    );
    Ok(())
}

fn main() -> Result<(), String> {
    println!("program,region,raw_bytes,deflate_bytes,median_decode_us,decode_mib_per_second,min_decode_us,max_decode_us");
    let paths: Vec<_> = std::env::args_os().skip(1).collect();
    if paths.is_empty() {
        return Err("usage: aot_compression NATIVE_BLOB...".into());
    }
    for path in paths {
        let bytes = std::fs::read(&path).map_err(|error| error.to_string())?;
        let native = lumen_common::aot::NativeContainer::parse(&bytes).map_err(str::to_owned)?;
        let name = path.to_string_lossy();
        measure(&name, "container", &bytes)?;
        for section in &native.sections {
            if !section.data.is_empty() {
                measure(&name, &format!("section-{}", section.kind), section.data)?;
            }
        }
    }
    Ok(())
}
