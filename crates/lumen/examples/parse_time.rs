//! Time the load-time parse of a JavaScript file: `parse_time FILE [script|cjs|module|lex] [REPEATS]`.
use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let path = args.next().ok_or("usage: parse_time FILE [script|cjs|module|lex] [REPEATS]")?;
    let mode = args.next().unwrap_or_else(|| "script".into());
    let repeats: usize = args.next().map_or(Ok(5), |n| n.parse())?;
    let source = std::fs::read_to_string(&path)?;
    let mut best = f64::MAX;
    for _ in 0..repeats {
        let start = Instant::now();
        let result = match mode.as_str() {
            "script" => lumen::check_script_syntax(&source),
            "cjs" => lumen::check_syntax(&source, false),
            #[cfg(feature = "bench")]
            "lex" => {
                std::hint::black_box(lumen::bench_api::tokenize_goal(&source, true).map(|t| t.len()))
                    .map(|_| ())
                    .map_err(|e| lumen::ParseError {
                        message: e.message,
                        line: e.line,
                        at_eof: false,
                    })
            }
            _ => lumen::check_syntax(&source, true),
        };
        if let Err(e) = result {
            return Err(format!("{}:{}: {}", path, e.line, e.message).into());
        }
        best = best.min(start.elapsed().as_secs_f64());
    }
    let mb = source.len() as f64 / 1e6;
    println!("{path} {mode}: best {:.1} ms, {:.1} MB/s", best * 1e3, mb / best);
    Ok(())
}
