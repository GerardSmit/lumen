//! Compile a JavaScript source file to an AST snapshot for embedding.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let source = args.next().ok_or("usage: compile_snapshot SOURCE OUTPUT")?;
    let output = args.next().ok_or("usage: compile_snapshot SOURCE OUTPUT")?;
    if args.next().is_some() {
        return Err("usage: compile_snapshot SOURCE OUTPUT".into());
    }
    let source = std::fs::read_to_string(source)?;
    let bytes = lumen::compile_snapshot(&source)?;
    std::fs::write(output, bytes)?;
    Ok(())
}
