//! Runtime loading of system shared libraries with `dlopen`, and the process-wide handle on the
//! system OpenSSL that every Lumen crate shares (`lumen-tls` for TLS, `lumen-crypto` for the
//! asymmetric and bignum operations). Nothing here links against OpenSSL; when it cannot be
//! loaded the callers fall back to their pure-Rust implementations.

use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_void};
use std::sync::OnceLock;

/// A `dlopen` handle. Handles obtained from [`openssl_crypto`] / [`openssl_ssl`] live for the whole
/// process; a `Library` opened directly closes itself on drop.
pub struct Library {
    handle: *mut c_void,
}

unsafe impl Send for Library {}
unsafe impl Sync for Library {}

impl Library {
    /// Opens the first of `paths` that loads; `what` names the library in the error.
    pub fn open_candidates(what: &str, paths: &[&str]) -> Result<Self, String> {
        let mut errors = Vec::new();
        for path in paths {
            match Self::open(path) {
                Ok(lib) => return Ok(lib),
                Err(error) => errors.push(error),
            }
        }
        Err(format!("no compatible {what} library found: {}", errors.join("; ")))
    }

    pub fn open(path: &str) -> Result<Self, String> {
        let path = CString::new(path).map_err(|_| "library path contains NUL".to_string())?;
        let handle = unsafe { dlopen(path.as_ptr(), 2) };
        if handle.is_null() {
            Err(format!("cannot load {}", path.to_string_lossy()))
        } else {
            Ok(Self { handle })
        }
    }

    /// The symbol `name` as a function pointer (or other `Copy` pointer-sized value) of type `T`.
    ///
    /// # Safety
    /// `T` must be the symbol's real type.
    pub unsafe fn function<T: Copy>(&self, name: &str) -> Result<T, String> {
        let name = CString::new(name).map_err(|_| "symbol contains NUL".to_string())?;
        let symbol = dlsym(self.handle, name.as_ptr());
        if symbol.is_null() {
            return Err(format!("missing symbol {}", name.to_string_lossy()));
        }
        Ok(std::mem::transmute_copy(&symbol))
    }
}

impl Drop for Library {
    fn drop(&mut self) {
        unsafe {
            dlclose(self.handle);
        }
    }
}

extern "C" {
    fn dlopen(path: *const c_char, mode: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn dlclose(handle: *mut c_void) -> c_int;
}

fn ssl_candidates() -> &'static [&'static str] {
    #[cfg(target_os = "macos")]
    {
        &["/opt/homebrew/lib/libssl.3.dylib", "/usr/local/lib/libssl.3.dylib", "libssl.3.dylib"]
    }
    #[cfg(target_os = "linux")]
    {
        &["libssl.so.3", "libssl.so"]
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        &[]
    }
}

fn crypto_candidates() -> &'static [&'static str] {
    #[cfg(target_os = "macos")]
    {
        &["/opt/homebrew/lib/libcrypto.3.dylib", "/usr/local/lib/libcrypto.3.dylib", "libcrypto.3.dylib"]
    }
    #[cfg(target_os = "linux")]
    {
        &["libcrypto.so.3", "libcrypto.so"]
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        &[]
    }
}

/// The system `libcrypto` (OpenSSL 3), loaded once for the process.
pub fn openssl_crypto() -> Result<&'static Library, &'static str> {
    static CRYPTO: OnceLock<Result<Library, String>> = OnceLock::new();
    CRYPTO.get_or_init(|| Library::open_candidates("OpenSSL", crypto_candidates())).as_ref().map_err(String::as_str)
}

/// The system `libssl` (OpenSSL 3), loaded and initialised once for the process; its `libcrypto`
/// is the one [`openssl_crypto`] returns.
pub fn openssl_ssl() -> Result<&'static Library, &'static str> {
    static SSL: OnceLock<Result<Library, String>> = OnceLock::new();
    SSL.get_or_init(|| {
        openssl_crypto().map_err(str::to_string)?;
        let ssl = Library::open_candidates("OpenSSL", ssl_candidates())?;
        unsafe {
            let init: unsafe extern "C" fn(u64, *const c_void) -> c_int = ssl.function("OPENSSL_init_ssl")?;
            if init(0, std::ptr::null()) != 1 {
                return Err("OPENSSL_init_ssl failed".into());
            }
        }
        Ok(ssl)
    })
    .as_ref()
    .map_err(String::as_str)
}
