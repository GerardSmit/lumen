const STACK_SIZE: usize = 1 << 28;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = std::thread::Builder::new()
        .stack_size(STACK_SIZE)
        .spawn(move || lumen_py::run_main(&args))
        .expect("spawn interpreter thread")
        .join()
        .unwrap_or(1);
    std::process::exit(code);
}
