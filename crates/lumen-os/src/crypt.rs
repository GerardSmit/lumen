//! The system `crypt(3)`, for the traditional DES formats that `lumen_common::crypt` does not
//! implement. The function is found at run time (libc itself, or `libcrypt`), since not every
//! libc links it. Where there is none, or off Unix, lookups fail with `ENOSYS`.

use crate::errno::FsError;

pub type R<T> = Result<T, FsError>;

#[cfg(all(unix, not(target_os = "android")))]
mod imp {
    use super::*;
    use std::ffi::{c_char, c_void, CStr, CString};
    use std::sync::{Mutex, OnceLock};

    type CryptFn = unsafe extern "C" fn(*const c_char, *const c_char) -> *mut c_char;

    fn find() -> Option<CryptFn> {
        // SAFETY: dlsym/dlopen take NUL-terminated names; a found `crypt` has this signature.
        unsafe {
            let sym = libc::dlsym(libc::RTLD_DEFAULT, c"crypt".as_ptr());
            if !sym.is_null() {
                return Some(std::mem::transmute::<*mut c_void, CryptFn>(sym));
            }
            for lib in [c"libcrypt.so.2", c"libcrypt.so.1", c"libcrypt.so", c"libcrypt.dylib"] {
                let h = libc::dlopen(lib.as_ptr(), libc::RTLD_NOW);
                if h.is_null() {
                    continue;
                }
                let sym = libc::dlsym(h, c"crypt".as_ptr());
                if !sym.is_null() {
                    return Some(std::mem::transmute::<*mut c_void, CryptFn>(sym));
                }
            }
        }
        None
    }

    static CRYPT: OnceLock<Option<CryptFn>> = OnceLock::new();
    static LOCK: Mutex<()> = Mutex::new(());

    pub fn crypt(password: &[u8], setting: &[u8]) -> R<Vec<u8>> {
        let f = CRYPT.get_or_init(find).ok_or(FsError("ENOSYS"))?;
        let word = CString::new(password).map_err(|_| FsError("EINVAL"))?;
        let salt = CString::new(setting).map_err(|_| FsError("EINVAL"))?;
        // crypt returns static storage, so calls are serialized and the result copied at once.
        let _guard = LOCK.lock().unwrap_or_else(|p| p.into_inner());
        // SAFETY: both arguments are NUL-terminated; the result is null or a C string.
        let out = unsafe { f(word.as_ptr(), salt.as_ptr()) };
        if out.is_null() {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: a non-null result is a NUL-terminated string.
        Ok(unsafe { CStr::from_ptr(out) }.to_bytes().to_vec())
    }
}

#[cfg(not(all(unix, not(target_os = "android"))))]
mod imp {
    use super::*;

    pub fn crypt(_password: &[u8], _setting: &[u8]) -> R<Vec<u8>> {
        Err(FsError("ENOSYS"))
    }
}

/// `crypt(3)` of `password` with `setting` (a DES salt or an `$id$...` prefix); the encrypted
/// text. `EINVAL` when the system function rejects the setting.
pub fn crypt(password: &[u8], setting: &[u8]) -> R<Vec<u8>> {
    imp::crypt(password, setting)
}
