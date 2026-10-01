//! `jsloop <which> <iters>`: one JS loop calling an op, for instruction counts.
use lumen::Engine;
use lumen_ext_demo::install;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let which = args.get(1).map(String::as_str).unwrap_or("clamp");
    let n: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(5_000_000);
    let mut e = Engine::new();
    install(&mut e);
    let call = match which {
        "clamp" => "ext.clamp(i, 10, 1000)",
        "blen" => "ext.blen(b)",
        "greet" => "ext.greet('x').length",
        "status" => "r.status",
        "empty" => "(i | 0)",
        _ => panic!("unknown"),
    };
    let src = format!(
        "(() => {{ const b = new Uint8Array(16); const r = new Response('x'); let acc = 0; for (let i = 0; i < {n}; i++) {{ acc += {call}; }} return acc; }})()"
    );
    let r = e.eval_value(&src).unwrap();
    assert!(r.is_ok(), "threw");
}
