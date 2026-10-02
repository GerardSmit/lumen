#[global_allocator]
static ALLOC: lumen_common::fastalloc::ClassAlloc = lumen_common::fastalloc::ClassAlloc;

const STACK_SIZE: usize = 1 << 28;

fn main() {
    lumen_os::signal::ignore_file_size_limit_signal();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let interp = std::thread::Builder::new()
        .stack_size(STACK_SIZE)
        .spawn(move || lumen_py::run_main(&args))
        .expect("spawn interpreter thread");
    lumen_os::signal::block_on_this_thread();
    let code = interp.join().unwrap_or(1);
    std::process::exit(code);
}
