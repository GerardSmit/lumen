//! Static runtime entry for linker-produced standalone apps. The data object
//! supplies immutable blob boundary symbols; the native loader owns placement.

use std::ffi::{c_char, CStr};

unsafe extern "C" {
    static lumen_aot_blob_start: u8;
    static lumen_aot_blob_end: u8;
    static lumen_aot_code_start: u8;
    static lumen_aot_code_end: u8;
    static mut lumen_aot_got_start: u8;
    static lumen_aot_got_end: u8;
}

/// C runtime startup owns the argument buffers until this function returns.
///
/// # Safety
/// `argv` must contain `argc` valid NUL-terminated C strings. The linked object
/// must supply a contiguous immutable blob between its start/end symbols.
#[no_mangle]
pub unsafe extern "C" fn main(argc: i32, argv: *const *const c_char) -> i32 {
    if argc < 0 || (argc > 0 && argv.is_null()) {
        eprintln!("invalid standalone process arguments");
        return 1;
    }
    let mut args = Vec::with_capacity(argc as usize);
    for index in 0..argc as usize {
        // SAFETY: argc/argv are checked above and supplied by the C runtime.
        let pointer = unsafe { *argv.add(index) };
        if pointer.is_null() {
            eprintln!("invalid standalone process argument");
            return 1;
        }
        args.push(
            unsafe { CStr::from_ptr(pointer) }
                .to_string_lossy()
                .into_owned(),
        );
    }
    let worker = std::thread::Builder::new()
        .name("lumen-main".into())
        .stack_size(256 * 1024 * 1024)
        .spawn(move || run(args));
    match worker.and_then(|worker| {
        worker
            .join()
            .map_err(|_| std::io::Error::other("standalone runtime panicked"))
    }) {
        Ok(Ok(status)) => status,
        Ok(Err(message)) => {
            eprintln!("{message}");
            1
        }
        Err(error) => {
            eprintln!("{error}");
            1
        }
    }
}

fn run(args: Vec<String>) -> Result<i32, String> {
    let start = std::ptr::addr_of!(lumen_aot_blob_start);
    let end = std::ptr::addr_of!(lumen_aot_blob_end);
    let length = (end as usize)
        .checked_sub(start as usize)
        .filter(|length| *length > 0)
        .ok_or("invalid standalone blob boundaries")?;
    // SAFETY: the linked data object's named section is immutable for process lifetime.
    let blob = unsafe { std::slice::from_raw_parts(start, length) };
    let mut runtime = lumen_runtime::Runtime::new();
    runtime.install_embedded_assets(blob)?;
    runtime.set_process_args(
        args.first().map_or("<embedded>", String::as_str),
        &[],
        &args,
    );
    if blob
        .get(8..12)
        .is_some_and(|bytes| bytes == 7u32.to_le_bytes())
    {
        let code = std::ptr::addr_of!(lumen_aot_code_start);
        let code_end = std::ptr::addr_of!(lumen_aot_code_end);
        let code_len = (code_end as usize)
            .checked_sub(code as usize)
            .ok_or("invalid linked code boundaries")?;
        if code_len == 0 {
            runtime.run_native_owned(blob.into())?;
        } else {
            let got = std::ptr::addr_of_mut!(lumen_aot_got_start);
            let got_end = std::ptr::addr_of!(lumen_aot_got_end);
            let got_bytes = (got_end as usize)
                .checked_sub(got as usize)
                .ok_or("invalid linked GOT boundaries")?;
            if got as usize % std::mem::align_of::<usize>() != 0
                || got_bytes % std::mem::size_of::<usize>() != 0
            {
                return Err("misaligned linked GOT".into());
            }
            unsafe {
                runtime.run_native_linked(
                    blob.into(),
                    code,
                    code_len,
                    got.cast(),
                    got_bytes / std::mem::size_of::<usize>(),
                )
            }?;
        }
    } else {
        #[cfg(feature = "compiler")]
        runtime.run_precompiled(&lumen::Precompiled::from_static(blob))?;
        #[cfg(not(feature = "compiler"))]
        return Err("the Aot standalone runtime requires a native payload".into());
    }
    runtime.run_to_completion();
    Ok(runtime.finish_process())
}
