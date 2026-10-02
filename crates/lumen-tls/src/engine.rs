//! Transport-free TLS state machines over the system OpenSSL (loaded with `dlopen`, never linked).
//!
//! A [`Context`] is an `SSL_CTX` (certificates, keys, trust store, protocol limits); a [`Session`]
//! is an `SSL` whose network side is a pair of memory BIOs: encrypted bytes from the peer go in
//! through [`Session::feed`], encrypted bytes for the peer come out of [`Session::take_output`],
//! and the owner moves them over whatever carries the connection. Nothing here blocks or
//! touches a socket, so it can be driven from an event loop.
//!
//! OpenSSL's decision callbacks (client hello, certificate selection, session storage, ALPN)
//! cannot call back into the embedder from inside a handshake, so they either record an
//! [`Event`] for the owner to drain after the call, or suspend the handshake with a retry code
//! the owner resolves by calling the matching `*_done` method.

#![cfg(unix)]

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_long, c_uint, c_ulong, c_void};
use std::sync::{Mutex, OnceLock};

use crate::openssl::{crypto_candidates, ssl_candidates, Library};

type Ptr = *mut c_void;
type CPtr = *const c_void;

type VerifyCallback = unsafe extern "C" fn(c_int, Ptr) -> c_int;
type InfoCallback = unsafe extern "C" fn(CPtr, c_int, c_int);
type NewSessionCallback = unsafe extern "C" fn(Ptr, Ptr) -> c_int;
type GetSessionCallback = unsafe extern "C" fn(Ptr, *const u8, c_int, *mut c_int) -> Ptr;
type ClientHelloCallback = unsafe extern "C" fn(Ptr, *mut c_int, Ptr) -> c_int;
type CertCallback = unsafe extern "C" fn(Ptr, Ptr) -> c_int;
type AlpnSelectCallback =
    unsafe extern "C" fn(Ptr, *mut *const u8, *mut u8, *const u8, c_uint, Ptr) -> c_int;
type KeylogCallback = unsafe extern "C" fn(CPtr, *const c_char);
type StatusCallback = unsafe extern "C" fn(Ptr, Ptr) -> c_int;

macro_rules! openssl_api {
    (@sel ssl, $s:ident, $c:ident, $n:expr) => { $s.function($n)? };
    (@sel crypto, $s:ident, $c:ident, $n:expr) => { $c.function($n)? };
    ($( $lib:ident $name:ident ( $($arg:ty),* ) $(-> $ret:ty)? ;)*) => {
        #[allow(non_snake_case)]
        struct Api {
            $( $name: unsafe extern "C" fn($($arg),*) $(-> $ret)?, )*
            _ssl: Library,
            _crypto: Library,
        }
        impl Api {
            fn load() -> Result<Api, String> {
                let crypto = Library::open_candidates(crypto_candidates())?;
                let ssl = Library::open_candidates(ssl_candidates())?;
                unsafe {
                    let init: unsafe extern "C" fn(u64, CPtr) -> c_int = ssl.function("OPENSSL_init_ssl")?;
                    if init(0, std::ptr::null()) != 1 {
                        return Err("OPENSSL_init_ssl failed".into());
                    }
                    Ok(Api {
                        $( $name: openssl_api!(@sel $lib, ssl, crypto, stringify!($name)), )*
                        _ssl: ssl,
                        _crypto: crypto,
                    })
                }
            }
        }
    };
}

openssl_api! {
    ssl TLS_method() -> CPtr;
    ssl SSL_CTX_new(CPtr) -> Ptr;
    ssl SSL_CTX_free(Ptr);
    ssl SSL_CTX_ctrl(Ptr, c_int, c_long, Ptr) -> c_long;
    ssl SSL_CTX_set_options(Ptr, u64) -> u64;
    ssl SSL_CTX_set_cipher_list(Ptr, *const c_char) -> c_int;
    ssl SSL_CTX_set_ciphersuites(Ptr, *const c_char) -> c_int;
    ssl SSL_CTX_set_verify(Ptr, c_int, Option<VerifyCallback>);
    ssl SSL_CTX_get_cert_store(CPtr) -> Ptr;
    ssl SSL_CTX_set_cert_store(Ptr, Ptr);
    ssl SSL_CTX_use_certificate(Ptr, Ptr) -> c_int;
    ssl SSL_CTX_use_PrivateKey(Ptr, Ptr) -> c_int;
    ssl SSL_CTX_check_private_key(CPtr) -> c_int;
    ssl SSL_CTX_add_client_CA(Ptr, Ptr) -> c_int;
    ssl SSL_CTX_set_session_id_context(Ptr, *const u8, c_uint) -> c_int;
    ssl SSL_CTX_set_timeout(Ptr, c_long) -> c_long;
    ssl SSL_CTX_sess_set_new_cb(Ptr, Option<NewSessionCallback>);
    ssl SSL_CTX_sess_set_get_cb(Ptr, Option<GetSessionCallback>);
    ssl SSL_CTX_set_client_hello_cb(Ptr, Option<ClientHelloCallback>, Ptr);
    ssl SSL_CTX_set_cert_cb(Ptr, Option<CertCallback>, Ptr);
    ssl SSL_CTX_set_alpn_select_cb(Ptr, Option<AlpnSelectCallback>, Ptr);
    ssl SSL_CTX_set_keylog_callback(Ptr, Option<KeylogCallback>);
    ssl SSL_CTX_get0_certificate(CPtr) -> Ptr;
    ssl SSL_CTX_get0_privatekey(CPtr) -> Ptr;
    ssl SSL_CTX_callback_ctrl(Ptr, c_int, Option<unsafe extern "C" fn()>) -> c_long;
    ssl SSL_CTX_get_client_CA_list(CPtr) -> Ptr;
    ssl SSL_CTX_get_options(CPtr) -> u64;
    ssl SSL_new(Ptr) -> Ptr;
    ssl SSL_free(Ptr);
    ssl SSL_ctrl(Ptr, c_int, c_long, Ptr) -> c_long;
    ssl SSL_set_bio(Ptr, Ptr, Ptr);
    ssl SSL_set_connect_state(Ptr);
    ssl SSL_set_accept_state(Ptr);
    ssl SSL_do_handshake(Ptr) -> c_int;
    ssl SSL_read(Ptr, Ptr, c_int) -> c_int;
    ssl SSL_write(Ptr, CPtr, c_int) -> c_int;
    ssl SSL_shutdown(Ptr) -> c_int;
    ssl SSL_get_error(CPtr, c_int) -> c_int;
    ssl SSL_set_verify(Ptr, c_int, Option<VerifyCallback>);
    ssl SSL_set_info_callback(Ptr, Option<InfoCallback>);
    ssl SSL_set_ex_data(Ptr, c_int, Ptr) -> c_int;
    ssl SSL_get_ex_data(CPtr, c_int) -> Ptr;
    ssl SSL_get_version(CPtr) -> *const c_char;
    ssl SSL_get_current_cipher(CPtr) -> CPtr;
    ssl SSL_CIPHER_get_name(CPtr) -> *const c_char;
    ssl SSL_CIPHER_standard_name(CPtr) -> *const c_char;
    ssl SSL_CIPHER_get_version(CPtr) -> *const c_char;
    ssl SSL_get_ciphers(CPtr) -> Ptr;
    ssl SSL_get1_peer_certificate(CPtr) -> Ptr;
    ssl SSL_get_peer_cert_chain(CPtr) -> Ptr;
    ssl SSL_get_certificate(CPtr) -> Ptr;
    ssl SSL_get_verify_result(CPtr) -> c_long;
    ssl SSL_get_servername(CPtr, c_int) -> *const c_char;
    ssl SSL_set_alpn_protos(Ptr, *const u8, c_uint) -> c_int;
    ssl SSL_get0_alpn_selected(CPtr, *mut *const u8, *mut c_uint);
    ssl SSL_select_next_proto(*mut *mut u8, *mut u8, *const u8, c_uint, *const u8, c_uint) -> c_int;
    ssl SSL_get1_session(Ptr) -> Ptr;
    ssl SSL_get_session(CPtr) -> Ptr;
    ssl SSL_set_session(Ptr, Ptr) -> c_int;
    ssl SSL_session_reused(CPtr) -> c_int;
    ssl SSL_SESSION_free(Ptr);
    ssl SSL_SESSION_get_id(CPtr, *mut c_uint) -> *const u8;
    ssl d2i_SSL_SESSION(*mut Ptr, *mut *const u8, c_long) -> Ptr;
    ssl i2d_SSL_SESSION(Ptr, *mut *mut u8) -> c_int;
    ssl SSL_get_finished(CPtr, Ptr, usize) -> usize;
    ssl SSL_get_peer_finished(CPtr, Ptr, usize) -> usize;
    ssl SSL_export_keying_material(Ptr, *mut u8, usize, *const c_char, usize, *const u8, usize, c_int) -> c_int;
    ssl SSL_set_SSL_CTX(Ptr, Ptr) -> Ptr;
    ssl SSL_use_certificate(Ptr, Ptr) -> c_int;
    ssl SSL_use_PrivateKey(Ptr, Ptr) -> c_int;
    ssl SSL_set_client_CA_list(Ptr, Ptr);
    ssl SSL_renegotiate(Ptr) -> c_int;
    ssl SSL_renegotiate_pending(CPtr) -> c_int;
    ssl SSL_pending(CPtr) -> c_int;
    ssl SSL_client_hello_get0_session_id(Ptr, *mut *const u8) -> usize;
    ssl SSL_client_hello_get0_ext(Ptr, c_uint, *mut *const u8, *mut usize) -> c_int;
    ssl SSL_set_options(Ptr, u64) -> u64;
    ssl SSL_get_shared_sigalgs(Ptr, c_int, *mut c_int, *mut c_int, *mut c_int, *mut u8, *mut u8) -> c_int;
    ssl SSL_get_verify_mode(CPtr) -> c_int;
    ssl SSL_get_shutdown(CPtr) -> c_int;
    ssl SSL_get_security_level(CPtr) -> c_int;
    ssl SSL_is_init_finished(CPtr) -> c_int;
    ssl SSL_dup_CA_list(CPtr) -> Ptr;
    ssl SSL_CTX_clear_options(Ptr, u64) -> u64;
    ssl SSL_CTX_get_verify_mode(CPtr) -> c_int;
    ssl SSL_CTX_load_verify_locations(Ptr, *const c_char, *const c_char) -> c_int;
    ssl SSL_CTX_set_default_verify_paths(Ptr) -> c_int;
    ssl SSL_CTX_get0_param(CPtr) -> Ptr;
    ssl SSL_get0_param(Ptr) -> Ptr;
    ssl SSL_CTX_get_security_level(CPtr) -> c_int;
    ssl SSL_set_post_handshake_auth(Ptr, c_int);
    ssl SSL_verify_client_post_handshake(Ptr) -> c_int;
    ssl SSL_CTX_set_num_tickets(Ptr, usize) -> c_int;
    ssl SSL_CTX_get_num_tickets(CPtr) -> usize;
    ssl SSL_get_client_ciphers(CPtr) -> Ptr;
    ssl SSL_CIPHER_get_bits(CPtr, *mut c_int) -> c_int;
    ssl SSL_CIPHER_get_id(CPtr) -> u32;
    ssl SSL_CIPHER_description(CPtr, *mut c_char, c_int) -> *mut c_char;
    ssl SSL_CIPHER_get_kx_nid(CPtr) -> c_int;
    ssl SSL_CIPHER_get_auth_nid(CPtr) -> c_int;
    ssl SSL_CIPHER_get_cipher_nid(CPtr) -> c_int;
    ssl SSL_CIPHER_get_digest_nid(CPtr) -> c_int;
    ssl SSL_CIPHER_is_aead(CPtr) -> c_int;
    ssl SSL_get_current_compression(CPtr) -> CPtr;
    ssl SSL_SESSION_get_time(CPtr) -> c_long;
    ssl SSL_SESSION_get_timeout(CPtr) -> c_long;
    ssl SSL_SESSION_has_ticket(CPtr) -> c_int;
    ssl SSL_SESSION_get_ticket_lifetime_hint(CPtr) -> c_ulong;
    ssl SSL_callback_ctrl(Ptr, c_int, Option<unsafe extern "C" fn()>) -> c_long;
    crypto BIO_ctrl(Ptr, c_int, c_long, Ptr) -> c_long;
    crypto d2i_X509_bio(Ptr, *mut Ptr) -> Ptr;
    crypto X509_check_ca(CPtr) -> c_int;
    crypto X509_VERIFY_PARAM_set_flags(Ptr, c_ulong) -> c_int;
    crypto X509_VERIFY_PARAM_clear_flags(Ptr, c_ulong) -> c_int;
    crypto X509_VERIFY_PARAM_get_flags(Ptr) -> c_ulong;
    crypto X509_VERIFY_PARAM_set_hostflags(Ptr, c_uint);
    crypto X509_VERIFY_PARAM_set1_host(Ptr, *const c_char, usize) -> c_int;
    crypto X509_VERIFY_PARAM_set1_ip_asc(Ptr, *const c_char) -> c_int;
    crypto X509_get_default_cert_file_env() -> *const c_char;
    crypto X509_get_default_cert_dir_env() -> *const c_char;
    crypto OBJ_nid2ln(c_int) -> *const c_char;
    crypto OBJ_nid2sn(c_int) -> *const c_char;
    crypto OBJ_sn2nid(*const c_char) -> c_int;
    crypto OBJ_nid2obj(c_int) -> CPtr;
    crypto OBJ_txt2obj(*const c_char, c_int) -> Ptr;
    crypto OBJ_obj2nid(CPtr) -> c_int;
    crypto OBJ_obj2txt(*mut c_char, c_int, CPtr, c_int) -> c_int;
    crypto ASN1_OBJECT_free(Ptr);
    crypto COMP_get_type(CPtr) -> c_int;
    crypto OpenSSL_version_num() -> c_ulong;
    crypto OpenSSL_version(c_int) -> *const c_char;
    crypto BIO_s_mem() -> CPtr;
    crypto BIO_new(CPtr) -> Ptr;
    crypto BIO_new_mem_buf(CPtr, c_int) -> Ptr;
    crypto BIO_free(Ptr) -> c_int;
    crypto BIO_read(Ptr, Ptr, c_int) -> c_int;
    crypto BIO_write(Ptr, CPtr, c_int) -> c_int;
    crypto BIO_ctrl_pending(Ptr) -> usize;
    crypto PEM_read_bio_X509(Ptr, *mut Ptr, CPtr, Ptr) -> Ptr;
    crypto PEM_read_bio_X509_AUX(Ptr, *mut Ptr, CPtr, Ptr) -> Ptr;
    crypto PEM_read_bio_PrivateKey(Ptr, *mut Ptr, CPtr, Ptr) -> Ptr;
    crypto PEM_read_bio_X509_CRL(Ptr, *mut Ptr, CPtr, Ptr) -> Ptr;
    crypto PEM_read_bio_DHparams(Ptr, *mut Ptr, CPtr, Ptr) -> Ptr;
    crypto d2i_X509(*mut Ptr, *mut *const u8, c_long) -> Ptr;
    crypto i2d_X509(CPtr, *mut *mut u8) -> c_int;
    crypto X509_free(Ptr);
    crypto X509_up_ref(Ptr) -> c_int;
    crypto X509_dup(CPtr) -> Ptr;
    crypto X509_check_issued(CPtr, CPtr) -> c_int;
    crypto X509_CRL_free(Ptr);
    crypto EVP_PKEY_free(Ptr);
    crypto DH_free(Ptr);
    crypto DH_get0_pqg(CPtr, *mut CPtr, *mut CPtr, *mut CPtr);
    crypto BN_num_bits(CPtr) -> c_int;
    crypto X509_STORE_new() -> Ptr;
    crypto X509_STORE_free(Ptr);
    crypto X509_STORE_up_ref(Ptr) -> c_int;
    crypto X509_STORE_add_cert(Ptr, Ptr) -> c_int;
    crypto X509_STORE_add_crl(Ptr, Ptr) -> c_int;
    crypto X509_STORE_set_flags(Ptr, c_ulong) -> c_int;
    crypto X509_STORE_set_default_paths(Ptr) -> c_int;
    crypto X509_STORE_load_file(Ptr, *const c_char) -> c_int;
    crypto X509_STORE_get0_objects(CPtr) -> Ptr;
    crypto X509_OBJECT_get_type(CPtr) -> c_int;
    crypto X509_OBJECT_get0_X509(CPtr) -> Ptr;
    crypto X509_STORE_CTX_new() -> Ptr;
    crypto X509_STORE_CTX_init(Ptr, Ptr, Ptr, Ptr) -> c_int;
    crypto X509_STORE_CTX_get1_issuer(*mut Ptr, Ptr, Ptr) -> c_int;
    crypto X509_STORE_CTX_free(Ptr);
    crypto d2i_PKCS12_bio(Ptr, *mut Ptr) -> Ptr;
    crypto PKCS12_parse(Ptr, *const c_char, *mut Ptr, *mut Ptr, *mut Ptr) -> c_int;
    crypto PKCS12_free(Ptr);
    crypto OPENSSL_sk_new_null() -> Ptr;
    crypto OPENSSL_sk_num(CPtr) -> c_int;
    crypto OPENSSL_sk_value(CPtr, c_int) -> Ptr;
    crypto OPENSSL_sk_push(Ptr, CPtr) -> c_int;
    crypto OPENSSL_sk_free(Ptr);
    crypto CRYPTO_malloc(usize, *const c_char, c_int) -> Ptr;
    crypto CRYPTO_free(Ptr, *const c_char, c_int);
    crypto EVP_PKEY_get_bits(CPtr) -> c_int;
    crypto EVP_PKEY_get_base_id(CPtr) -> c_int;
    crypto ERR_get_error() -> c_ulong;
    crypto ERR_peek_error() -> c_ulong;
    crypto ERR_peek_last_error() -> c_ulong;
    crypto ERR_clear_error();
    crypto ERR_error_string_n(c_ulong, *mut c_char, usize);
    crypto ERR_lib_error_string(c_ulong) -> *const c_char;
    crypto ERR_reason_error_string(c_ulong) -> *const c_char;
    crypto X509_verify_cert_error_string(c_long) -> *const c_char;
    crypto X509_get_default_cert_file() -> *const c_char;
    crypto X509_get_default_cert_dir() -> *const c_char;
}

fn api() -> Result<&'static Api, String> {
    static API: OnceLock<Result<Api, String>> = OnceLock::new();
    API.get_or_init(Api::load).as_ref().map_err(Clone::clone)
}

// OpenSSL is not re-entrant per object but the library itself is thread-safe; every Context and
// Session here is owned by one thread at a time.
unsafe impl Sync for Api {}
unsafe impl Send for Api {}

pub const TLS1_VERSION: i32 = 0x301;
pub const TLS1_1_VERSION: i32 = 0x302;
pub const TLS1_2_VERSION: i32 = 0x303;
pub const TLS1_3_VERSION: i32 = 0x304;

const SSL_CTRL_SET_TMP_DH: c_int = 3;
const SSL_CTRL_MODE: c_int = 33;
const SSL_CTRL_SET_SESS_CACHE_MODE: c_int = 44;
const SSL_CTRL_SET_MAX_SEND_FRAGMENT: c_int = 52;
const SSL_CTRL_SET_TLSEXT_HOSTNAME: c_int = 55;
const SSL_CTRL_GET_TLSEXT_TICKET_KEYS: c_int = 58;
const SSL_CTRL_SET_TLSEXT_TICKET_KEYS: c_int = 59;
const SSL_CTRL_SET_TLSEXT_STATUS_REQ_CB: c_int = 63;
const SSL_CTRL_SET_TLSEXT_STATUS_REQ_TYPE: c_int = 65;
const SSL_CTRL_GET_TLSEXT_STATUS_REQ_OCSP_RESP: c_int = 70;
const SSL_CTRL_SET_TLSEXT_STATUS_REQ_OCSP_RESP: c_int = 71;
const SSL_CTRL_CLEAR_EXTRA_CHAIN_CERTS: c_int = 83;
const SSL_CTRL_CHAIN: c_int = 88;
const SSL_CTRL_CHAIN_CERT: c_int = 89;
const SSL_CTRL_GET_CHAIN_CERTS: c_int = 115;
const SSL_CTRL_SET_GROUPS_LIST: c_int = 92;
const SSL_CTRL_SET_SIGALGS_LIST: c_int = 98;
const SSL_CTRL_GET_PEER_TMP_KEY: c_int = 109;
const SSL_CTRL_SET_DH_AUTO: c_int = 118;
const SSL_CTRL_SET_MIN_PROTO_VERSION: c_int = 123;
const SSL_CTRL_SET_MAX_PROTO_VERSION: c_int = 124;
const SSL_CTRL_GET_TLSEXT_STATUS_REQ_TYPE: c_int = 127;
const SSL_CTRL_GET_MIN_PROTO_VERSION: c_int = 130;
const SSL_CTRL_GET_MAX_PROTO_VERSION: c_int = 131;
const SSL_CTRL_CLEAR_MODE: c_int = 78;

const SSL_MODE_AUTO_RETRY: c_long = 4;
const SSL_MODE_NO_AUTO_CHAIN: c_long = 8;
const SSL_MODE_RELEASE_BUFFERS: c_long = 0x10;
const SSL_OP_ALLOW_CLIENT_RENEGOTIATION: u64 = 1 << 8;
const SSL_OP_NO_SSLV3: u64 = 1 << 25;

const SSL_VERIFY_PEER: c_int = 1;
const SSL_VERIFY_FAIL_IF_NO_PEER_CERT: c_int = 2;

const X509_V_FLAG_CRL_CHECK: c_ulong = 0x4;
const X509_V_FLAG_CRL_CHECK_ALL: c_ulong = 0x8;
const X509_LU_X509: c_int = 1;

const TLSEXT_STATUSTYPE_OCSP: c_long = 1;
const SSL_CB_HANDSHAKE_START: c_int = 0x10;
const SSL_CB_HANDSHAKE_DONE: c_int = 0x20;

const ERR_LIB_PEM: c_ulong = 9;
const PEM_R_NO_START_LINE: c_ulong = 108;
const SSL_R_NO_CIPHER_MATCH: c_ulong = 121;

pub const SSL_ERROR_SSL: i32 = 1;
pub const SSL_ERROR_WANT_READ: i32 = 2;
pub const SSL_ERROR_WANT_WRITE: i32 = 3;
pub const SSL_ERROR_WANT_X509_LOOKUP: i32 = 4;
pub const SSL_ERROR_SYSCALL: i32 = 5;
pub const SSL_ERROR_ZERO_RETURN: i32 = 6;
pub const SSL_ERROR_WANT_CLIENT_HELLO_CB: i32 = 11;

/// A failure carrying OpenSSL's error-queue detail (or just a message).
#[derive(Debug, Clone, Default)]
pub struct EngineError {
    pub message: String,
    pub library: Option<String>,
    pub function: Option<String>,
    pub reason: Option<String>,
    pub code: Option<String>,
    /// The JS error class: `Error` unless the failure is a usage error.
    pub kind: Option<&'static str>,
    /// The oldest and newest raw OpenSSL error codes of the queue entry that produced this error.
    pub raw_first: u64,
    pub raw_last: u64,
}

impl EngineError {
    pub fn plain(message: impl Into<String>) -> Self {
        EngineError {
            message: message.into(),
            ..Default::default()
        }
    }
    pub fn with_code(message: impl Into<String>, code: &str, kind: &'static str) -> Self {
        EngineError {
            message: message.into(),
            code: Some(code.to_string()),
            kind: Some(kind),
            ..Default::default()
        }
    }
}

impl From<String> for EngineError {
    fn from(message: String) -> Self {
        EngineError::plain(message)
    }
}

fn lib_prefix(lib: c_ulong) -> &'static str {
    match lib {
        2 => "SYS_",
        3 => "BN_",
        4 => "RSA_",
        5 => "DH_",
        6 => "EVP_",
        7 => "BUF_",
        8 => "OBJ_",
        9 => "PEM_",
        10 => "DSA_",
        11 => "X509_",
        13 => "ASN1_",
        14 => "CONF_",
        15 => "CRYPTO_",
        16 => "EC_",
        20 => "SSL_",
        32 => "BIO_",
        33 => "PKCS7_",
        34 => "X509V3_",
        35 => "PKCS12_",
        36 => "RAND_",
        37 => "DSO_",
        38 => "ENGINE_",
        39 => "OCSP_",
        40 => "UI_",
        41 => "COMP_",
        42 => "ECDSA_",
        43 => "ECDH_",
        44 => "OSSL_STORE_",
        45 => "FIPS_",
        46 => "CMS_",
        47 => "TS_",
        48 => "HMAC_",
        50 => "CT_",
        51 => "ASYNC_",
        52 => "KDF_",
        53 => "SM2_",
        128 => "USER_",
        _ => "",
    }
}

fn err_lib(code: c_ulong) -> c_ulong {
    if code & (1 << 31) != 0 {
        2
    } else {
        (code >> 23) & 0xff
    }
}

fn err_reason(code: c_ulong) -> c_ulong {
    if code & (1 << 31) != 0 {
        code & 0x7fffff
    } else {
        code & 0x7fffff
    }
}

fn cstr(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        None
    } else {
        Some(unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned())
    }
}

impl Api {
    fn error_string(&self, code: c_ulong) -> String {
        let mut buffer = [0 as c_char; 256];
        unsafe { (self.ERR_error_string_n)(code, buffer.as_mut_ptr(), buffer.len()) };
        cstr(buffer.as_ptr()).unwrap_or_default()
    }

    /// Drain the error queue into one error: the message is the newest entry, the library and
    /// reason come from the oldest, as Node reports them.
    fn take_error(&self, fallback: &str) -> EngineError {
        self.take_error_with(fallback, false)
    }

    /// `handshake` errors report the newest queue entry as the message and an `ERR_SSL_` code,
    /// the way a failed handshake does; others report the oldest entry and tag the code by library.
    fn take_error_with(&self, fallback: &str, handshake: bool) -> EngineError {
        let first = unsafe { (self.ERR_peek_error)() };
        let mut last = first;
        loop {
            let code = unsafe { (self.ERR_get_error)() };
            if code == 0 {
                break;
            }
            last = code;
        }
        if first == 0 {
            return EngineError::plain(fallback);
        }
        let message = self.error_string(if handshake { last } else { first });
        let library = cstr(unsafe { (self.ERR_lib_error_string)(first) });
        let reason = cstr(unsafe { (self.ERR_reason_error_string)(first) });
        let code = reason.as_ref().map(|reason| {
            let mut name = String::from("ERR_");
            if handshake {
                name.push_str("SSL_");
                name.extend(reason.chars().map(|c| if c == ' ' { '_' } else { c.to_ascii_uppercase() }));
                return name;
            }
            let lib = err_lib(first);
            name.push_str(if lib == 20 { "SSL_" } else { "OSSL_" });
            if lib != 20 {
                let prefix = lib_prefix(lib);
                if prefix != "" {
                    name.push_str(prefix);
                }
            }
            for c in reason.chars() {
                name.push(if c == ' ' { '_' } else { c.to_ascii_uppercase() });
            }
            name
        });
        EngineError {
            message,
            library,
            function: None,
            reason,
            code,
            kind: None,
            raw_first: first as u64,
            raw_last: last as u64,
        }
    }

    fn peek_is_no_start_line(&self) -> bool {
        let err = unsafe { (self.ERR_peek_last_error)() };
        err_lib(err) == ERR_LIB_PEM && err_reason(err) == PEM_R_NO_START_LINE
    }

    fn mem_bio(&self, bytes: &[u8]) -> Result<Ptr, EngineError> {
        let length = c_int::try_from(bytes.len()).map_err(|_| EngineError::plain("input is too large"))?;
        let bio = unsafe { (self.BIO_new_mem_buf)(bytes.as_ptr() as CPtr, length) };
        if bio.is_null() {
            Err(EngineError::plain("BIO_new_mem_buf failed"))
        } else {
            Ok(bio)
        }
    }

    fn der_of(&self, cert: CPtr) -> Vec<u8> {
        if cert.is_null() {
            return Vec::new();
        }
        let size = unsafe { (self.i2d_X509)(cert, std::ptr::null_mut()) };
        if size <= 0 {
            return Vec::new();
        }
        let mut out = vec![0u8; size as usize];
        let mut cursor = out.as_mut_ptr();
        unsafe { (self.i2d_X509)(cert, &mut cursor) };
        out
    }
}

unsafe extern "C" fn accept_all(_preverify: c_int, _store: Ptr) -> c_int {
    1
}

/// The CA bundle files OpenSSL's compiled-in defaults may miss on systems that keep them elsewhere.
const FALLBACK_BUNDLES: &[&str] = &[
    "/etc/ssl/cert.pem",
    "/etc/ssl/certs/ca-certificates.crt",
    "/etc/pki/tls/certs/ca-bundle.crt",
    "/etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem",
    "/opt/homebrew/etc/ca-certificates/cert.pem",
    "/usr/local/etc/openssl@3/cert.pem",
    "/opt/homebrew/etc/openssl@3/cert.pem",
];

/// The PEM text of each trusted root the platform ships, in file order.
pub fn root_certificate_pems() -> Vec<String> {
    let mut candidates: Vec<String> = Vec::new();
    if let Ok(file) = std::env::var("SSL_CERT_FILE") {
        candidates.push(file);
    }
    if let Ok(api) = api() {
        if let Some(file) = cstr(unsafe { (api.X509_get_default_cert_file)() }) {
            candidates.push(file);
        }
    }
    candidates.extend(FALLBACK_BUNDLES.iter().map(|path| path.to_string()));
    for path in candidates {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let mut pems = Vec::new();
        let mut rest = text.as_str();
        while let Some(start) = rest.find("-----BEGIN CERTIFICATE-----") {
            let Some(end) = rest[start..].find("-----END CERTIFICATE-----") else {
                break;
            };
            let end = start + end + "-----END CERTIFICATE-----".len();
            let mut pem = rest[start..end].to_string();
            pem.push('\n');
            pems.push(pem);
            rest = &rest[end..];
        }
        if !pems.is_empty() {
            return pems;
        }
    }
    Vec::new()
}

struct SendPtr(usize);

fn root_store() -> Result<Ptr, EngineError> {
    static STORE: OnceLock<Mutex<Option<SendPtr>>> = OnceLock::new();
    let guard = STORE.get_or_init(|| Mutex::new(None));
    let mut slot = guard.lock().unwrap();
    if let Some(store) = slot.as_ref() {
        return Ok(store.0 as Ptr);
    }
    let api = api()?;
    let store = new_root_store(api)?;
    *slot = Some(SendPtr(store as usize));
    Ok(store)
}

fn load_pem_file_into(api: &Api, store: Ptr, path: &str) -> Result<(), c_ulong> {
    let Ok(bytes) = std::fs::read(path) else {
        return Ok(());
    };
    let bio = api.mem_bio(&bytes).map_err(|_| 0 as c_ulong)?;
    unsafe { (api.ERR_clear_error)() };
    loop {
        let x509 = unsafe {
            (api.PEM_read_bio_X509)(bio, std::ptr::null_mut(), std::ptr::null(), std::ptr::null_mut())
        };
        if x509.is_null() {
            break;
        }
        unsafe {
            (api.X509_STORE_add_cert)(store, x509);
            (api.X509_free)(x509);
        }
    }
    let err = unsafe { (api.ERR_peek_error)() };
    unsafe {
        (api.BIO_free)(bio);
    }
    let no_start = err_lib(err) == ERR_LIB_PEM && err_reason(err) == PEM_R_NO_START_LINE;
    unsafe { (api.ERR_clear_error)() };
    if err == 0 || no_start {
        Ok(())
    } else {
        Err(err)
    }
}

fn new_root_store(api: &Api) -> Result<Ptr, EngineError> {
    let store = unsafe { (api.X509_STORE_new)() };
    if store.is_null() {
        return Err(api.take_error("X509_STORE_new failed"));
    }
    let pems = root_certificate_pems();
    if pems.is_empty() {
        unsafe { (api.X509_STORE_set_default_paths)(store) };
        unsafe { (api.ERR_clear_error)() };
    } else {
        for pem in &pems {
            let _ = load_pem_text(api, store, pem.as_bytes());
        }
    }
    if let Ok(extra) = std::env::var("NODE_EXTRA_CA_CERTS") {
        if !extra.is_empty() {
            match load_pem_file_into(api, store, &extra) {
                Ok(()) if std::path::Path::new(&extra).exists() => {}
                Ok(()) => eprintln!(
                    "Warning: Ignoring extra certs from `{extra}`, load failed: error:80000002:system library::No such file or directory"
                ),
                Err(err) => eprintln!(
                    "Warning: Ignoring extra certs from `{extra}`, load failed: {}",
                    api.error_string(err)
                ),
            }
        }
    }
    Ok(store)
}

fn load_pem_text(api: &Api, store: Ptr, bytes: &[u8]) -> Result<(), EngineError> {
    let bio = api.mem_bio(bytes)?;
    unsafe { (api.ERR_clear_error)() };
    loop {
        let x509 = unsafe {
            (api.PEM_read_bio_X509)(bio, std::ptr::null_mut(), std::ptr::null(), std::ptr::null_mut())
        };
        if x509.is_null() {
            break;
        }
        unsafe {
            (api.X509_STORE_add_cert)(store, x509);
            (api.X509_free)(x509);
        }
    }
    unsafe {
        (api.BIO_free)(bio);
        (api.ERR_clear_error)();
    }
    Ok(())
}

/// Whether `NODE_EXTRA_CA_CERTS` named a file that loaded.
pub fn extra_root_certs_loaded() -> bool {
    let Ok(extra) = std::env::var("NODE_EXTRA_CA_CERTS") else {
        return false;
    };
    !extra.is_empty() && std::path::Path::new(&extra).exists()
}

pub struct Context {
    api: &'static Api,
    ctx: Ptr,
    shared_store: bool,
    cert: Vec<u8>,
    issuer: Vec<u8>,
}

unsafe impl Send for Context {}

impl Drop for Context {
    fn drop(&mut self) {
        self.close();
    }
}

impl Context {
    pub fn new(method: Option<&str>, min_version: i32, max_version: i32) -> Result<Context, EngineError> {
        let api = api()?;
        let mut min_version = min_version;
        let mut max_version = if max_version == 0 { TLS1_3_VERSION } else { max_version };
        if let Some(method) = method {
            let invalid = |message: String| {
                EngineError::with_code(message, "ERR_TLS_INVALID_PROTOCOL_METHOD", "TypeError")
            };
            let (min, max) = match method {
                "SSLv2_method" | "SSLv2_server_method" | "SSLv2_client_method" => {
                    return Err(invalid("SSLv2 methods disabled".into()))
                }
                "SSLv3_method" | "SSLv3_server_method" | "SSLv3_client_method" => {
                    return Err(invalid("SSLv3 methods disabled".into()))
                }
                "SSLv23_method" | "SSLv23_server_method" | "SSLv23_client_method" => {
                    (min_version, TLS1_2_VERSION)
                }
                "TLS_method" | "TLS_server_method" | "TLS_client_method" => (0, TLS1_3_VERSION),
                "TLSv1_method" | "TLSv1_server_method" | "TLSv1_client_method" => {
                    (TLS1_VERSION, TLS1_VERSION)
                }
                "TLSv1_1_method" | "TLSv1_1_server_method" | "TLSv1_1_client_method" => {
                    (TLS1_1_VERSION, TLS1_1_VERSION)
                }
                "TLSv1_2_method" | "TLSv1_2_server_method" | "TLSv1_2_client_method" => {
                    (TLS1_2_VERSION, TLS1_2_VERSION)
                }
                other => return Err(invalid(format!("Unknown method: {other}"))),
            };
            min_version = min;
            max_version = max;
        }
        let ctx = unsafe { (api.SSL_CTX_new)((api.TLS_method)()) };
        if ctx.is_null() {
            return Err(api.take_error("SSL_CTX_new"));
        }
        unsafe {
            (api.SSL_CTX_set_options)(ctx, SSL_OP_NO_SSLV3 | SSL_OP_ALLOW_CLIENT_RENEGOTIATION);
            (api.SSL_CTX_ctrl)(ctx, SSL_CTRL_CLEAR_MODE, SSL_MODE_NO_AUTO_CHAIN, std::ptr::null_mut());
            // CLIENT | SERVER | NO_INTERNAL | NO_AUTO_CLEAR
            (api.SSL_CTX_ctrl)(ctx, SSL_CTRL_SET_SESS_CACHE_MODE, 0x0001 | 0x0002 | 0x0300 | 0x0080, std::ptr::null_mut());
            (api.SSL_CTX_ctrl)(ctx, SSL_CTRL_SET_MIN_PROTO_VERSION, min_version as c_long, std::ptr::null_mut());
            (api.SSL_CTX_ctrl)(ctx, SSL_CTRL_SET_MAX_PROTO_VERSION, max_version as c_long, std::ptr::null_mut());
            (api.SSL_CTX_sess_set_new_cb)(ctx, Some(new_session_callback));
            (api.SSL_CTX_sess_set_get_cb)(ctx, Some(get_session_callback));
            (api.SSL_CTX_set_client_hello_cb)(ctx, Some(client_hello_callback), std::ptr::null_mut());
            (api.SSL_CTX_set_cert_cb)(ctx, Some(cert_callback), std::ptr::null_mut());
            (api.SSL_CTX_set_alpn_select_cb)(ctx, Some(alpn_select_callback), std::ptr::null_mut());
            (api.SSL_CTX_set_keylog_callback)(ctx, Some(keylog_callback));
            let status: StatusCallback = status_callback;
            let status: unsafe extern "C" fn() = std::mem::transmute(status);
            (api.SSL_CTX_callback_ctrl)(ctx, SSL_CTRL_SET_TLSEXT_STATUS_REQ_CB, Some(status));
            (api.ERR_clear_error)();
        }
        Ok(Context {
            api,
            ctx,
            shared_store: false,
            cert: Vec::new(),
            issuer: Vec::new(),
        })
    }

    pub fn close(&mut self) {
        if !self.ctx.is_null() {
            unsafe { (self.api.SSL_CTX_free)(self.ctx) };
            self.ctx = std::ptr::null_mut();
        }
    }

    pub fn is_closed(&self) -> bool {
        self.ctx.is_null()
    }

    fn live(&self) -> Result<Ptr, EngineError> {
        if self.ctx.is_null() {
            Err(EngineError::plain("SecureContext is closed"))
        } else {
            Ok(self.ctx)
        }
    }

    pub fn set_key(&mut self, pem: &[u8], passphrase: Option<&[u8]>) -> Result<(), EngineError> {
        let api = self.api;
        let ctx = self.live()?;
        let bio = api.mem_bio(pem)?;
        unsafe { (api.ERR_clear_error)() };
        let pass = passphrase
            .map(|bytes| CString::new(bytes.iter().copied().filter(|b| *b != 0).collect::<Vec<u8>>()).unwrap());
        let key = unsafe {
            (api.PEM_read_bio_PrivateKey)(
                bio,
                std::ptr::null_mut(),
                std::ptr::null(),
                pass.as_ref().map_or(std::ptr::null_mut(), |p| p.as_ptr() as Ptr),
            )
        };
        unsafe { (api.BIO_free)(bio) };
        if key.is_null() {
            return Err(api.take_error("PEM_read_bio_PrivateKey"));
        }
        let ok = unsafe { (api.SSL_CTX_use_PrivateKey)(ctx, key) };
        unsafe { (api.EVP_PKEY_free)(key) };
        if ok != 1 {
            return Err(api.take_error("SSL_CTX_use_PrivateKey"));
        }
        Ok(())
    }

    pub fn set_cert(&mut self, pem: &[u8]) -> Result<(), EngineError> {
        let api = self.api;
        let ctx = self.live()?;
        self.cert.clear();
        self.issuer.clear();
        unsafe { (api.ERR_clear_error)() };
        let bio = api.mem_bio(pem)?;
        let leaf = unsafe {
            (api.PEM_read_bio_X509_AUX)(bio, std::ptr::null_mut(), std::ptr::null(), std::ptr::null_mut())
        };
        if leaf.is_null() {
            unsafe { (api.BIO_free)(bio) };
            return Err(api.take_error("SSL_CTX_use_certificate_chain"));
        }
        let mut extras: Vec<Ptr> = Vec::new();
        loop {
            let extra = unsafe {
                (api.PEM_read_bio_X509)(bio, std::ptr::null_mut(), std::ptr::null(), std::ptr::null_mut())
            };
            if extra.is_null() {
                break;
            }
            extras.push(extra);
        }
        unsafe { (api.BIO_free)(bio) };
        if !api.peek_is_no_start_line() {
            unsafe {
                (api.X509_free)(leaf);
                for extra in &extras {
                    (api.X509_free)(*extra);
                }
            }
            return Err(api.take_error("SSL_CTX_use_certificate_chain"));
        }
        unsafe { (api.ERR_clear_error)() };
        let result = self.use_chain(ctx, leaf, &extras);
        unsafe {
            (api.X509_free)(leaf);
            for extra in &extras {
                (api.X509_free)(*extra);
            }
        }
        result
    }

    fn use_chain(&mut self, ctx: Ptr, leaf: Ptr, extras: &[Ptr]) -> Result<(), EngineError> {
        let api = self.api;
        if unsafe { (api.SSL_CTX_use_certificate)(ctx, leaf) } != 1 {
            return Err(api.take_error("SSL_CTX_use_certificate_chain"));
        }
        unsafe { (api.SSL_CTX_ctrl)(ctx, SSL_CTRL_CLEAR_EXTRA_CHAIN_CERTS, 0, std::ptr::null_mut()) };
        let mut issuer: Ptr = std::ptr::null_mut();
        for extra in extras {
            if unsafe { (api.SSL_CTX_ctrl)(ctx, SSL_CTRL_CHAIN_CERT, 1, *extra) } != 1 {
                return Err(api.take_error("SSL_CTX_use_certificate_chain"));
            }
            if issuer.is_null() && unsafe { (api.X509_check_issued)(*extra, leaf) } == 0 {
                issuer = *extra;
            }
        }
        self.cert = api.der_of(leaf);
        self.issuer = if !issuer.is_null() {
            api.der_of(issuer)
        } else {
            self.lookup_issuer(leaf)
        };
        Ok(())
    }

    fn lookup_issuer(&self, leaf: Ptr) -> Vec<u8> {
        let api = self.api;
        let store = unsafe { (api.SSL_CTX_get_cert_store)(self.ctx) };
        if store.is_null() {
            return Vec::new();
        }
        let store_ctx = unsafe { (api.X509_STORE_CTX_new)() };
        if store_ctx.is_null() {
            return Vec::new();
        }
        let mut found: Ptr = std::ptr::null_mut();
        let mut der = Vec::new();
        unsafe {
            if (api.X509_STORE_CTX_init)(store_ctx, store, std::ptr::null_mut(), std::ptr::null_mut()) == 1
                && (api.X509_STORE_CTX_get1_issuer)(&mut found, store_ctx, leaf) == 1
                && !found.is_null()
            {
                der = api.der_of(found);
                (api.X509_free)(found);
            }
            (api.X509_STORE_CTX_free)(store_ctx);
            (api.ERR_clear_error)();
        }
        der
    }

    fn private_store(&mut self) -> Result<Ptr, EngineError> {
        let api = self.api;
        let ctx = self.live()?;
        if !self.shared_store {
            return Ok(unsafe { (api.SSL_CTX_get_cert_store)(ctx) });
        }
        let store = unsafe { (api.X509_STORE_new)() };
        if store.is_null() {
            return Err(api.take_error("X509_STORE_new"));
        }
        let shared = unsafe { (api.SSL_CTX_get_cert_store)(ctx) };
        let objects = unsafe { (api.X509_STORE_get0_objects)(shared) };
        let count = unsafe { (api.OPENSSL_sk_num)(objects) };
        for index in 0..count {
            let object = unsafe { (api.OPENSSL_sk_value)(objects, index) };
            if unsafe { (api.X509_OBJECT_get_type)(object) } == X509_LU_X509 {
                let cert = unsafe { (api.X509_OBJECT_get0_X509)(object) };
                unsafe { (api.X509_STORE_add_cert)(store, cert) };
            }
        }
        unsafe { (api.SSL_CTX_set_cert_store)(ctx, store) };
        self.shared_store = false;
        Ok(store)
    }

    pub fn add_ca_cert(&mut self, pem: &[u8]) -> Result<(), EngineError> {
        let api = self.api;
        let ctx = self.live()?;
        let bio = api.mem_bio(pem)?;
        unsafe { (api.ERR_clear_error)() };
        loop {
            let x509 = unsafe {
                (api.PEM_read_bio_X509_AUX)(bio, std::ptr::null_mut(), std::ptr::null(), std::ptr::null_mut())
            };
            if x509.is_null() {
                break;
            }
            let store = match self.private_store() {
                Ok(store) => store,
                Err(error) => {
                    unsafe {
                        (api.X509_free)(x509);
                        (api.BIO_free)(bio);
                    }
                    return Err(error);
                }
            };
            unsafe {
                (api.X509_STORE_add_cert)(store, x509);
                (api.SSL_CTX_add_client_CA)(ctx, x509);
                (api.X509_free)(x509);
            }
        }
        unsafe {
            (api.BIO_free)(bio);
            (api.ERR_clear_error)();
        }
        Ok(())
    }

    pub fn add_crl(&mut self, pem: &[u8]) -> Result<(), EngineError> {
        let api = self.api;
        self.live()?;
        let bio = api.mem_bio(pem)?;
        unsafe { (api.ERR_clear_error)() };
        let crl = unsafe {
            (api.PEM_read_bio_X509_CRL)(bio, std::ptr::null_mut(), std::ptr::null(), std::ptr::null_mut())
        };
        unsafe { (api.BIO_free)(bio) };
        if crl.is_null() {
            unsafe { (api.ERR_clear_error)() };
            return Err(EngineError::with_code(
                "Failed to parse CRL",
                "ERR_CRYPTO_OPERATION_FAILED",
                "Error",
            ));
        }
        let store = self.private_store()?;
        unsafe {
            (api.X509_STORE_add_crl)(store, crl);
            (api.X509_STORE_set_flags)(store, X509_V_FLAG_CRL_CHECK | X509_V_FLAG_CRL_CHECK_ALL);
            (api.X509_CRL_free)(crl);
        }
        Ok(())
    }

    pub fn add_root_certs(&mut self) -> Result<(), EngineError> {
        let api = self.api;
        let ctx = self.live()?;
        let store = root_store()?;
        unsafe {
            (api.X509_STORE_up_ref)(store);
            (api.SSL_CTX_set_cert_store)(ctx, store);
        }
        self.shared_store = true;
        Ok(())
    }

    pub fn set_cipher_suites(&mut self, list: &str) -> Result<(), EngineError> {
        let api = self.api;
        let ctx = self.live()?;
        let list = CString::new(list).map_err(|_| EngineError::plain("invalid cipher list"))?;
        unsafe { (api.ERR_clear_error)() };
        if unsafe { (api.SSL_CTX_set_ciphersuites)(ctx, list.as_ptr()) } != 1 {
            let mut error = api.take_error("Failed to set ciphers");
            error.message = error.message.clone();
            return Err(error);
        }
        Ok(())
    }

    pub fn set_ciphers(&mut self, list: &str) -> Result<(), EngineError> {
        let api = self.api;
        let ctx = self.live()?;
        let c_list = CString::new(list).map_err(|_| EngineError::plain("invalid cipher list"))?;
        unsafe { (api.ERR_clear_error)() };
        if unsafe { (api.SSL_CTX_set_cipher_list)(ctx, c_list.as_ptr()) } != 1 {
            let first = unsafe { (api.ERR_peek_error)() };
            if list.is_empty() && err_reason(first) == SSL_R_NO_CIPHER_MATCH {
                unsafe { (api.ERR_clear_error)() };
                return Ok(());
            }
            return Err(api.take_error("Failed to set ciphers"));
        }
        Ok(())
    }

    pub fn set_sigalgs(&mut self, list: &str) -> Result<(), EngineError> {
        let api = self.api;
        let ctx = self.live()?;
        let list = CString::new(list).map_err(|_| EngineError::plain("invalid sigalgs"))?;
        unsafe { (api.ERR_clear_error)() };
        if unsafe { (api.SSL_CTX_ctrl)(ctx, SSL_CTRL_SET_SIGALGS_LIST, 0, list.as_ptr() as Ptr) } != 1 {
            return Err(api.take_error("SSL_CTX_set1_sigalgs_list"));
        }
        Ok(())
    }

    pub fn set_ecdh_curve(&mut self, curve: &str) -> Result<(), EngineError> {
        let api = self.api;
        let ctx = self.live()?;
        if curve == "auto" {
            return Ok(());
        }
        let c_curve = CString::new(curve).map_err(|_| EngineError::plain("invalid curve"))?;
        unsafe { (api.ERR_clear_error)() };
        if unsafe { (api.SSL_CTX_ctrl)(ctx, SSL_CTRL_SET_GROUPS_LIST, 0, c_curve.as_ptr() as Ptr) } != 1 {
            unsafe { (api.ERR_clear_error)() };
            return Err(EngineError::with_code(
                "Failed to set ECDH curve",
                "ERR_CRYPTO_OPERATION_FAILED",
                "Error",
            ));
        }
        Ok(())
    }

    /// Returns a warning for parameters under 2048 bits.
    pub fn set_dh_param(&mut self, pem: Option<&[u8]>) -> Result<Option<&'static str>, EngineError> {
        let api = self.api;
        let ctx = self.live()?;
        let Some(pem) = pem else {
            unsafe { (api.SSL_CTX_ctrl)(ctx, SSL_CTRL_SET_DH_AUTO, 1, std::ptr::null_mut()) };
            return Ok(None);
        };
        let bio = api.mem_bio(pem)?;
        unsafe { (api.ERR_clear_error)() };
        let dh = unsafe {
            (api.PEM_read_bio_DHparams)(bio, std::ptr::null_mut(), std::ptr::null(), std::ptr::null_mut())
        };
        unsafe { (api.BIO_free)(bio) };
        if dh.is_null() {
            unsafe { (api.ERR_clear_error)() };
            return Ok(None);
        }
        let mut p: CPtr = std::ptr::null();
        unsafe { (api.DH_get0_pqg)(dh, &mut p, std::ptr::null_mut(), std::ptr::null_mut()) };
        let bits = unsafe { (api.BN_num_bits)(p) };
        if bits < 1024 {
            unsafe { (api.DH_free)(dh) };
            return Err(EngineError::with_code(
                "DH parameter is less than 1024 bits",
                "ERR_INVALID_ARG_VALUE",
                "TypeError",
            ));
        }
        let warning = if bits < 2048 {
            Some("DH parameter is less than 2048 bits")
        } else {
            None
        };
        let ok = unsafe { (api.SSL_CTX_ctrl)(ctx, SSL_CTRL_SET_TMP_DH, 0, dh) };
        unsafe { (api.DH_free)(dh) };
        if ok != 1 {
            unsafe { (api.ERR_clear_error)() };
            return Err(EngineError::with_code(
                "Error setting temp DH parameter",
                "ERR_CRYPTO_OPERATION_FAILED",
                "Error",
            ));
        }
        Ok(warning)
    }

    pub fn set_min_proto(&mut self, version: i32) -> Result<(), EngineError> {
        let ctx = self.live()?;
        unsafe { (self.api.SSL_CTX_ctrl)(ctx, SSL_CTRL_SET_MIN_PROTO_VERSION, version as c_long, std::ptr::null_mut()) };
        Ok(())
    }

    pub fn set_max_proto(&mut self, version: i32) -> Result<(), EngineError> {
        let ctx = self.live()?;
        unsafe { (self.api.SSL_CTX_ctrl)(ctx, SSL_CTRL_SET_MAX_PROTO_VERSION, version as c_long, std::ptr::null_mut()) };
        Ok(())
    }

    pub fn min_proto(&self) -> i32 {
        if self.ctx.is_null() {
            return 0;
        }
        unsafe { (self.api.SSL_CTX_ctrl)(self.ctx, SSL_CTRL_GET_MIN_PROTO_VERSION, 0, std::ptr::null_mut()) as i32 }
    }

    pub fn max_proto(&self) -> i32 {
        if self.ctx.is_null() {
            return 0;
        }
        unsafe { (self.api.SSL_CTX_ctrl)(self.ctx, SSL_CTRL_GET_MAX_PROTO_VERSION, 0, std::ptr::null_mut()) as i32 }
    }

    pub fn set_options(&mut self, options: u64) -> Result<(), EngineError> {
        let ctx = self.live()?;
        unsafe { (self.api.SSL_CTX_set_options)(ctx, options) };
        Ok(())
    }

    pub fn set_session_id_context(&mut self, context: &[u8]) -> Result<(), EngineError> {
        let api = self.api;
        let ctx = self.live()?;
        unsafe { (api.ERR_clear_error)() };
        if unsafe { (api.SSL_CTX_set_session_id_context)(ctx, context.as_ptr(), context.len() as c_uint) } != 1 {
            let mut error = api.take_error("SSL_CTX_set_session_id_context error");
            error.kind = Some("TypeError");
            return Err(error);
        }
        Ok(())
    }

    pub fn set_session_timeout(&mut self, seconds: i32) -> Result<(), EngineError> {
        let ctx = self.live()?;
        unsafe { (self.api.SSL_CTX_set_timeout)(ctx, seconds as c_long) };
        Ok(())
    }

    pub fn set_ticket_keys(&mut self, keys: &[u8]) -> Result<(), EngineError> {
        let ctx = self.live()?;
        if keys.len() != 48 {
            return Err(EngineError::plain("Session ticket keys must be a 48-byte buffer"));
        }
        unsafe {
            (self.api.SSL_CTX_ctrl)(ctx, SSL_CTRL_SET_TLSEXT_TICKET_KEYS, 48, keys.as_ptr() as Ptr)
        };
        Ok(())
    }

    pub fn ticket_keys(&self) -> Vec<u8> {
        let mut keys = vec![0u8; 48];
        if !self.ctx.is_null() {
            unsafe {
                (self.api.SSL_CTX_ctrl)(
                    self.ctx,
                    SSL_CTRL_GET_TLSEXT_TICKET_KEYS,
                    48,
                    keys.as_mut_ptr() as Ptr,
                )
            };
        }
        keys
    }

    pub fn load_pkcs12(&mut self, data: &[u8], passphrase: Option<&[u8]>) -> Result<(), EngineError> {
        let api = self.api;
        let ctx = self.live()?;
        unsafe { (api.ERR_clear_error)() };
        self.cert.clear();
        self.issuer.clear();
        let bio = api.mem_bio(data)?;
        let p12 = unsafe { (api.d2i_PKCS12_bio)(bio, std::ptr::null_mut()) };
        unsafe { (api.BIO_free)(bio) };
        let fail = |api: &Api| {
            let first = unsafe { (api.ERR_peek_error)() };
            let reason = cstr(unsafe { (api.ERR_reason_error_string)(first) });
            unsafe { (api.ERR_clear_error)() };
            EngineError::plain(reason.unwrap_or_else(|| "Unknown error".to_string()))
        };
        if p12.is_null() {
            return Err(fail(api));
        }
        let pass = CString::new(
            passphrase
                .unwrap_or(&[])
                .iter()
                .copied()
                .filter(|b| *b != 0)
                .collect::<Vec<u8>>(),
        )
        .unwrap();
        let (mut key, mut cert, mut chain): (Ptr, Ptr, Ptr) =
            (std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut());
        let parsed = unsafe { (api.PKCS12_parse)(p12, pass.as_ptr(), &mut key, &mut cert, &mut chain) };
        unsafe { (api.PKCS12_free)(p12) };
        if parsed != 1 {
            return Err(fail(api));
        }
        let free_all = |key: Ptr, cert: Ptr, chain: Ptr| unsafe {
            if !key.is_null() {
                (api.EVP_PKEY_free)(key);
            }
            if !cert.is_null() {
                (api.X509_free)(cert);
            }
            if !chain.is_null() {
                for index in 0..(api.OPENSSL_sk_num)(chain) {
                    (api.X509_free)((api.OPENSSL_sk_value)(chain, index));
                }
                (api.OPENSSL_sk_free)(chain);
            }
        };
        if key.is_null() {
            free_all(key, cert, chain);
            return Err(EngineError::with_code(
                "Unable to load private key from PFX data",
                "ERR_CRYPTO_OPERATION_FAILED",
                "Error",
            ));
        }
        if cert.is_null() {
            free_all(key, cert, chain);
            return Err(EngineError::with_code(
                "Unable to load certificate from PFX data",
                "ERR_CRYPTO_OPERATION_FAILED",
                "Error",
            ));
        }
        let extras: Vec<Ptr> = if chain.is_null() {
            Vec::new()
        } else {
            (0..unsafe { (api.OPENSSL_sk_num)(chain) })
                .map(|index| unsafe { (api.OPENSSL_sk_value)(chain, index) })
                .collect()
        };
        let mut result = self.use_chain(ctx, cert, &extras).map_err(|_| fail(api));
        if result.is_ok() && unsafe { (api.SSL_CTX_use_PrivateKey)(ctx, key) } != 1 {
            result = Err(fail(api));
        }
        if result.is_ok() {
            for extra in &extras {
                match self.private_store() {
                    Ok(store) => unsafe {
                        (api.X509_STORE_add_cert)(store, *extra);
                        (api.SSL_CTX_add_client_CA)(ctx, *extra);
                    },
                    Err(error) => {
                        result = Err(error);
                        break;
                    }
                }
            }
        }
        free_all(key, cert, chain);
        result
    }

    pub fn certificate(&self) -> &[u8] {
        &self.cert
    }

    pub fn issuer(&self) -> &[u8] {
        &self.issuer
    }

    pub fn check_private_key(&self) -> bool {
        !self.ctx.is_null() && unsafe { (self.api.SSL_CTX_check_private_key)(self.ctx) } == 1
    }
}

/// Names of the ciphers a default client offers, as OpenSSL spells them.
pub fn cipher_names() -> Result<Vec<String>, EngineError> {
    let api = api()?;
    let ctx = unsafe { (api.SSL_CTX_new)((api.TLS_method)()) };
    if ctx.is_null() {
        return Err(api.take_error("SSL_CTX_new"));
    }
    let ssl = unsafe { (api.SSL_new)(ctx) };
    let mut names = Vec::new();
    if !ssl.is_null() {
        let stack = unsafe { (api.SSL_get_ciphers)(ssl) };
        if !stack.is_null() {
            for index in 0..unsafe { (api.OPENSSL_sk_num)(stack) } {
                let cipher = unsafe { (api.OPENSSL_sk_value)(stack, index) };
                if let Some(name) = cstr(unsafe { (api.SSL_CIPHER_get_name)(cipher) }) {
                    names.push(name);
                }
            }
        }
        unsafe { (api.SSL_free)(ssl) };
    }
    unsafe { (api.SSL_CTX_free)(ctx) };
    Ok(names)
}

pub fn openssl_available() -> bool {
    api().is_ok()
}

/// Something the handshake machinery did that the owner must act on.
pub enum Event {
    HandshakeStart,
    HandshakeDone,
    NewSession { id: Vec<u8>, session: Vec<u8> },
    Keylog(Vec<u8>),
    OcspResponse(Option<Vec<u8>>),
}

#[derive(Default, Clone)]
pub struct ClientHello {
    pub session_id: Vec<u8>,
    pub servername: String,
    pub has_ticket: bool,
    pub alpn: Vec<u8>,
    pub ocsp_request: bool,
}

/// One protocol message OpenSSL reported through its message callback.
pub struct Message {
    pub write: bool,
    pub version: i32,
    pub content_type: i32,
    pub data: Vec<u8>,
}

#[derive(Default)]
struct State {
    events: Vec<Event>,
    session_callbacks: bool,
    keylog: bool,
    pause_hello: bool,
    hello_reported: bool,
    hello_done: bool,
    hello: Option<ClientHello>,
    cert_cb: bool,
    cert_reported: bool,
    cert_done: bool,
    cert_info: Option<(String, bool)>,
    alpn_protos: Vec<u8>,
    alpn_callback: bool,
    alpn_choice: Option<usize>,
    ocsp_response: Option<Vec<u8>>,
    next_session: Ptr,
    is_server: bool,
    hello_alert: Option<i32>,
    alpn_noack: bool,
    messages: Vec<Message>,
}

pub struct Session {
    api: &'static Api,
    ssl: Ptr,
    rbio: Ptr,
    wbio: Ptr,
    state: Box<State>,
    is_server: bool,
}

unsafe impl Send for Session {}

fn state_of<'a>(api: &Api, ssl: CPtr) -> Option<&'a mut State> {
    let pointer = unsafe { (api.SSL_get_ex_data)(ssl, 0) } as *mut State;
    if pointer.is_null() {
        None
    } else {
        Some(unsafe { &mut *pointer })
    }
}

fn global_api() -> &'static Api {
    api().expect("OpenSSL was loaded before a callback ran")
}

unsafe extern "C" fn info_callback(ssl: CPtr, where_: c_int, _ret: c_int) {
    if where_ & (SSL_CB_HANDSHAKE_START | SSL_CB_HANDSHAKE_DONE) == 0 {
        return;
    }
    let api = global_api();
    let Some(state) = state_of(api, ssl) else { return };
    if where_ & SSL_CB_HANDSHAKE_START != 0 {
        state.events.push(Event::HandshakeStart);
    }
    if where_ & SSL_CB_HANDSHAKE_DONE != 0 && unsafe { (api.SSL_renegotiate_pending)(ssl) } == 0 {
        state.events.push(Event::HandshakeDone);
    }
}

unsafe extern "C" fn new_session_callback(ssl: Ptr, session: Ptr) -> c_int {
    let api = global_api();
    let Some(state) = state_of(api, ssl) else { return 0 };
    if !state.session_callbacks {
        return 0;
    }
    let size = unsafe { (api.i2d_SSL_SESSION)(session, std::ptr::null_mut()) };
    if size <= 0 || size > 10 * 1024 {
        return 0;
    }
    let mut data = vec![0u8; size as usize];
    let mut cursor = data.as_mut_ptr();
    unsafe { (api.i2d_SSL_SESSION)(session, &mut cursor) };
    let mut id_length: c_uint = 0;
    let id = unsafe { (api.SSL_SESSION_get_id)(session, &mut id_length) };
    let id = if id.is_null() {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(id, id_length as usize) }.to_vec()
    };
    state.events.push(Event::NewSession { id, session: data });
    0
}

unsafe extern "C" fn get_session_callback(ssl: Ptr, _id: *const u8, _len: c_int, copy: *mut c_int) -> Ptr {
    let api = global_api();
    let Some(state) = state_of(api, ssl) else {
        return std::ptr::null_mut();
    };
    unsafe { *copy = 0 };
    std::mem::replace(&mut state.next_session, std::ptr::null_mut())
}

fn parse_hello(api: &Api, ssl: Ptr) -> ClientHello {
    let mut hello = ClientHello::default();
    let mut data: *const u8 = std::ptr::null();
    let length = unsafe { (api.SSL_client_hello_get0_session_id)(ssl, &mut data) };
    if !data.is_null() {
        hello.session_id = unsafe { std::slice::from_raw_parts(data, length) }.to_vec();
    }
    let mut len = 0usize;
    unsafe {
        let mut ext: *const u8 = std::ptr::null();
        if (api.SSL_client_hello_get0_ext)(ssl, 35, &mut ext, &mut len) == 1 {
            hello.has_ticket = len > 0;
        }
        if (api.SSL_client_hello_get0_ext)(ssl, 5, &mut ext, &mut len) == 1 {
            hello.ocsp_request = len > 0 && *ext == TLSEXT_STATUSTYPE_OCSP as u8;
        }
        if (api.SSL_client_hello_get0_ext)(ssl, 16, &mut ext, &mut len) == 1 && len >= 2 {
            let list = std::slice::from_raw_parts(ext, len);
            let declared = ((list[0] as usize) << 8) | list[1] as usize;
            if declared + 2 == len {
                hello.alpn = list[2..].to_vec();
            }
        }
        if (api.SSL_client_hello_get0_ext)(ssl, 0, &mut ext, &mut len) == 1 && len >= 5 {
            let list = std::slice::from_raw_parts(ext, len);
            let name_length = ((list[3] as usize) << 8) | list[4] as usize;
            if list[2] == 0 && 5 + name_length <= len {
                hello.servername = String::from_utf8_lossy(&list[5..5 + name_length]).into_owned();
            }
        }
    }
    hello
}

unsafe extern "C" fn client_hello_callback(ssl: Ptr, alert: *mut c_int, _arg: Ptr) -> c_int {
    let api = global_api();
    let Some(state) = state_of(api, ssl) else { return 1 };
    if state.hello_done || !state.is_server {
        if let Some(code) = state.hello_alert.take() {
            unsafe { *alert = code };
            return 0;
        }
        return 1;
    }
    if state.hello_reported {
        return -1;
    }
    state.hello = Some(parse_hello(api, ssl));
    if !state.pause_hello {
        state.hello_done = true;
        return 1;
    }
    state.hello_reported = true;
    -1
}

unsafe extern "C" fn cert_callback(ssl: Ptr, _arg: Ptr) -> c_int {
    let api = global_api();
    let Some(state) = state_of(api, ssl) else { return 1 };
    if !state.is_server || !state.cert_cb || state.cert_done {
        return 1;
    }
    if !state.cert_reported {
        state.cert_reported = true;
        let name = cstr(unsafe { (api.SSL_get_servername)(ssl, 0) }).unwrap_or_default();
        let ocsp = unsafe { (api.SSL_ctrl)(ssl, SSL_CTRL_GET_TLSEXT_STATUS_REQ_TYPE, 0, std::ptr::null_mut()) }
            == TLSEXT_STATUSTYPE_OCSP;
        state.cert_info = Some((name, ocsp));
    }
    -1
}

unsafe extern "C" fn alpn_select_callback(
    ssl: Ptr,
    out: *mut *const u8,
    out_length: *mut u8,
    input: *const u8,
    input_length: c_uint,
    _arg: Ptr,
) -> c_int {
    let api = global_api();
    let Some(state) = state_of(api, ssl) else { return 3 };
    if state.alpn_callback {
        let Some(offset) = state.alpn_choice else { return 2 };
        if offset >= input_length as usize {
            return 2;
        }
        unsafe {
            *out_length = *input.add(offset);
            *out = input.add(offset + 1);
        }
        return 0;
    }
    if state.alpn_protos.is_empty() {
        return 3;
    }
    let mut selected: *mut u8 = std::ptr::null_mut();
    let mut length: u8 = 0;
    let status = unsafe {
        (api.SSL_select_next_proto)(
            &mut selected,
            &mut length,
            state.alpn_protos.as_ptr(),
            state.alpn_protos.len() as c_uint,
            input,
            input_length,
        )
    };
    if status != 1 {
        return if state.alpn_noack { 3 } else { 2 };
    }
    unsafe {
        *out = selected;
        *out_length = length;
    }
    0
}

unsafe extern "C" fn keylog_callback(ssl: CPtr, line: *const c_char) {
    let api = global_api();
    let Some(state) = state_of(api, ssl) else { return };
    if !state.keylog {
        return;
    }
    let mut bytes = unsafe { CStr::from_ptr(line) }.to_bytes().to_vec();
    bytes.push(b'\n');
    state.events.push(Event::Keylog(bytes));
}

unsafe extern "C" fn status_callback(ssl: Ptr, _arg: Ptr) -> c_int {
    let api = global_api();
    let Some(state) = state_of(api, ssl) else { return 1 };
    if !state.is_server {
        let mut response: *mut u8 = std::ptr::null_mut();
        let length = unsafe {
            (api.SSL_ctrl)(
                ssl,
                SSL_CTRL_GET_TLSEXT_STATUS_REQ_OCSP_RESP,
                0,
                &mut response as *mut *mut u8 as Ptr,
            )
        };
        let bytes = if response.is_null() || length <= 0 {
            None
        } else {
            Some(unsafe { std::slice::from_raw_parts(response, length as usize) }.to_vec())
        };
        state.events.push(Event::OcspResponse(bytes));
        return 1;
    }
    let Some(response) = state.ocsp_response.take() else { return 3 };
    let data = unsafe { (api.CRYPTO_malloc)(response.len(), c"lumen".as_ptr(), 0) } as *mut u8;
    if data.is_null() {
        return 3;
    }
    unsafe { std::ptr::copy_nonoverlapping(response.as_ptr(), data, response.len()) };
    unsafe {
        (api.SSL_ctrl)(
            ssl,
            SSL_CTRL_SET_TLSEXT_STATUS_REQ_OCSP_RESP,
            response.len() as c_long,
            data as Ptr,
        )
    };
    0
}

pub enum Io {
    Data(Vec<u8>),
    Code(i32),
}

impl Session {
    pub fn new(context: &Context, is_server: bool) -> Result<Session, EngineError> {
        Session::create(context, is_server, true)
    }

    /// A session that keeps the verification mode and callback of its context, so a failed
    /// verification aborts the handshake as OpenSSL does (see [`Context::set_verify`]), where
    /// [`Session::new`] always completes the handshake and leaves the verdict to the owner.
    pub fn new_inherit(context: &Context, is_server: bool) -> Result<Session, EngineError> {
        Session::create(context, is_server, false)
    }

    fn create(context: &Context, is_server: bool, owner_verifies: bool) -> Result<Session, EngineError> {
        let api = context.api;
        let ctx = context.live()?;
        let ssl = unsafe { (api.SSL_new)(ctx) };
        if ssl.is_null() {
            return Err(api.take_error("SSL_new"));
        }
        let rbio = unsafe { (api.BIO_new)((api.BIO_s_mem)()) };
        let wbio = unsafe { (api.BIO_new)((api.BIO_s_mem)()) };
        if rbio.is_null() || wbio.is_null() {
            unsafe { (api.SSL_free)(ssl) };
            return Err(api.take_error("BIO_new"));
        }
        unsafe { (api.SSL_set_bio)(ssl, rbio, wbio) };
        let mut state = Box::new(State {
            is_server,
            next_session: std::ptr::null_mut(),
            ..Default::default()
        });
        unsafe {
            (api.SSL_set_ex_data)(ssl, 0, &mut *state as *mut State as Ptr);
            if owner_verifies {
                (api.SSL_set_verify)(ssl, 0, Some(accept_all));
            }
            (api.SSL_ctrl)(ssl, SSL_CTRL_MODE, SSL_MODE_AUTO_RETRY | SSL_MODE_RELEASE_BUFFERS | 2, std::ptr::null_mut());
            (api.SSL_set_info_callback)(ssl, Some(info_callback));
            if is_server {
                (api.SSL_set_accept_state)(ssl);
            } else {
                (api.SSL_set_connect_state)(ssl);
            }
            (api.ERR_clear_error)();
        }
        Ok(Session {
            api,
            ssl,
            rbio,
            wbio,
            state,
            is_server,
        })
    }

    pub fn is_server(&self) -> bool {
        self.is_server
    }

    pub fn feed(&mut self, bytes: &[u8]) -> bool {
        let mut offset = 0;
        while offset < bytes.len() {
            let chunk = (bytes.len() - offset).min(c_int::MAX as usize);
            let written = unsafe { (self.api.BIO_write)(self.rbio, bytes[offset..].as_ptr() as CPtr, chunk as c_int) };
            if written <= 0 {
                return false;
            }
            offset += written as usize;
        }
        true
    }

    pub fn pending_output(&self) -> usize {
        unsafe { (self.api.BIO_ctrl_pending)(self.wbio) }
    }

    pub fn take_output(&mut self) -> Vec<u8> {
        let pending = self.pending_output();
        let mut out = vec![0u8; pending];
        let mut filled = 0;
        while filled < pending {
            let read = unsafe {
                (self.api.BIO_read)(self.wbio, out[filled..].as_mut_ptr() as Ptr, (pending - filled) as c_int)
            };
            if read <= 0 {
                break;
            }
            filled += read as usize;
        }
        out.truncate(filled);
        out
    }

    pub fn read(&mut self, max: usize) -> Io {
        let mut buffer = vec![0u8; max.clamp(1, 1 << 20)];
        let read = unsafe { (self.api.SSL_read)(self.ssl, buffer.as_mut_ptr() as Ptr, buffer.len() as c_int) };
        if read > 0 {
            buffer.truncate(read as usize);
            return Io::Data(buffer);
        }
        Io::Code(unsafe { (self.api.SSL_get_error)(self.ssl, read) })
    }

    /// Bytes accepted (the whole buffer) or a negative SSL error code.
    pub fn write(&mut self, bytes: &[u8]) -> i64 {
        if bytes.is_empty() {
            return 0;
        }
        let written = unsafe { (self.api.SSL_write)(self.ssl, bytes.as_ptr() as CPtr, bytes.len() as c_int) };
        if written > 0 {
            written as i64
        } else {
            -(unsafe { (self.api.SSL_get_error)(self.ssl, written) } as i64)
        }
    }

    pub fn shutdown(&mut self) {
        unsafe {
            if (self.api.SSL_shutdown)(self.ssl) == 0 {
                (self.api.SSL_shutdown)(self.ssl);
            }
            (self.api.ERR_clear_error)();
        }
    }

    pub fn last_error(&self) -> EngineError {
        self.api.take_error_with("TLS error", true)
    }

    pub fn clear_errors(&self) {
        unsafe { (self.api.ERR_clear_error)() };
    }

    pub fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.state.events)
    }

    pub fn set_verify_mode(&mut self, request_cert: bool, reject_unauthorized: bool) {
        let mode = if self.is_server {
            if !request_cert {
                0
            } else if reject_unauthorized {
                SSL_VERIFY_PEER | SSL_VERIFY_FAIL_IF_NO_PEER_CERT
            } else {
                SSL_VERIFY_PEER
            }
        } else {
            0
        };
        unsafe { (self.api.SSL_set_verify)(self.ssl, mode, Some(accept_all)) };
    }

    /// The X509 verification result and its description, `None` when the chain verified.
    pub fn verify_error(&self) -> Option<(i32, String)> {
        let api = self.api;
        let peer = unsafe { (api.SSL_get1_peer_certificate)(self.ssl) };
        let result = if peer.is_null() {
            let cipher = unsafe { (api.SSL_get_current_cipher)(self.ssl) };
            let name = if cipher.is_null() {
                None
            } else {
                cstr(unsafe { (api.SSL_CIPHER_get_name)(cipher) })
            };
            if name.map_or(true, |name| name.contains("PSK")) {
                return None;
            }
            1
        } else {
            unsafe { (api.X509_free)(peer) };
            unsafe { (api.SSL_get_verify_result)(self.ssl) }
        };
        if result == 0 {
            return None;
        }
        let reason = cstr(unsafe { (api.X509_verify_cert_error_string)(result) }).unwrap_or_default();
        Some((result as i32, reason))
    }

    pub fn protocol(&self) -> Option<String> {
        cstr(unsafe { (self.api.SSL_get_version)(self.ssl) })
    }

    /// `(name, standard name, version)` of the negotiated cipher.
    pub fn cipher(&self) -> Option<(String, String, String)> {
        let cipher = unsafe { (self.api.SSL_get_current_cipher)(self.ssl) };
        if cipher.is_null() {
            return None;
        }
        Some((
            cstr(unsafe { (self.api.SSL_CIPHER_get_name)(cipher) }).unwrap_or_default(),
            cstr(unsafe { (self.api.SSL_CIPHER_standard_name)(cipher) }).unwrap_or_default(),
            cstr(unsafe { (self.api.SSL_CIPHER_get_version)(cipher) }).unwrap_or_default(),
        ))
    }

    pub fn alpn_selected(&self) -> Option<Vec<u8>> {
        let mut data: *const u8 = std::ptr::null();
        let mut length: c_uint = 0;
        unsafe { (self.api.SSL_get0_alpn_selected)(self.ssl, &mut data, &mut length) };
        if data.is_null() || length == 0 {
            None
        } else {
            Some(unsafe { std::slice::from_raw_parts(data, length as usize) }.to_vec())
        }
    }

    pub fn set_alpn_protocols(&mut self, protocols: &[u8]) -> bool {
        if self.is_server {
            self.state.alpn_protos = protocols.to_vec();
            true
        } else {
            unsafe { (self.api.SSL_set_alpn_protos)(self.ssl, protocols.as_ptr(), protocols.len() as c_uint) == 0 }
        }
    }

    pub fn servername(&self) -> Option<String> {
        cstr(unsafe { (self.api.SSL_get_servername)(self.ssl, 0) })
    }

    pub fn set_servername(&mut self, name: &str) -> bool {
        let Ok(name) = CString::new(name) else {
            return false;
        };
        unsafe { (self.api.SSL_ctrl)(self.ssl, SSL_CTRL_SET_TLSEXT_HOSTNAME, 0, name.as_ptr() as Ptr) == 1 }
    }

    pub fn session_bytes(&self) -> Option<Vec<u8>> {
        let api = self.api;
        let session = unsafe { (api.SSL_get1_session)(self.ssl) };
        if session.is_null() {
            return None;
        }
        let size = unsafe { (api.i2d_SSL_SESSION)(session, std::ptr::null_mut()) };
        let mut data = vec![0u8; size.max(0) as usize];
        if size > 0 {
            let mut cursor = data.as_mut_ptr();
            unsafe { (api.i2d_SSL_SESSION)(session, &mut cursor) };
        }
        unsafe { (api.SSL_SESSION_free)(session) };
        if size > 0 {
            Some(data)
        } else {
            None
        }
    }

    fn decode_session(&self, bytes: &[u8]) -> Ptr {
        let mut cursor = bytes.as_ptr();
        unsafe { (self.api.d2i_SSL_SESSION)(std::ptr::null_mut(), &mut cursor, bytes.len() as c_long) }
    }

    pub fn set_session(&mut self, bytes: &[u8]) -> bool {
        let session = self.decode_session(bytes);
        if session.is_null() {
            unsafe { (self.api.ERR_clear_error)() };
            return false;
        }
        let ok = unsafe { (self.api.SSL_set_session)(self.ssl, session) } == 1;
        unsafe { (self.api.SSL_SESSION_free)(session) };
        ok
    }

    pub fn load_session(&mut self, bytes: Option<&[u8]>) {
        if !self.state.next_session.is_null() {
            unsafe { (self.api.SSL_SESSION_free)(self.state.next_session) };
            self.state.next_session = std::ptr::null_mut();
        }
        if let Some(bytes) = bytes {
            self.state.next_session = self.decode_session(bytes);
            unsafe { (self.api.ERR_clear_error)() };
        }
    }

    pub fn session_reused(&self) -> bool {
        unsafe { (self.api.SSL_session_reused)(self.ssl) == 1 }
    }

    pub fn finished(&self, peer: bool) -> Option<Vec<u8>> {
        let mut buffer = vec![0u8; 2048];
        let size = unsafe {
            if peer {
                (self.api.SSL_get_peer_finished)(self.ssl, buffer.as_mut_ptr() as Ptr, buffer.len())
            } else {
                (self.api.SSL_get_finished)(self.ssl, buffer.as_mut_ptr() as Ptr, buffer.len())
            }
        };
        if size == 0 {
            return None;
        }
        buffer.truncate(size.min(2048));
        Some(buffer)
    }

    pub fn export_keying_material(&mut self, length: usize, label: &str, context: Option<&[u8]>) -> Result<Vec<u8>, EngineError> {
        let mut out = vec![0u8; length];
        let (context_ptr, context_len, use_context) = match context {
            Some(context) => (context.as_ptr(), context.len(), 1),
            None => (std::ptr::null(), 0, 0),
        };
        let ok = unsafe {
            (self.api.SSL_export_keying_material)(
                self.ssl,
                out.as_mut_ptr(),
                length,
                label.as_ptr() as *const c_char,
                label.len(),
                context_ptr,
                context_len,
                use_context,
            )
        };
        if ok != 1 {
            return Err(self.api.take_error("SSL_export_keying_material"));
        }
        Ok(out)
    }

    /// `(type, name, size)` of the ephemeral key a TLS key exchange used.
    pub fn peer_certificates(&self) -> Vec<Vec<u8>> {
        let api = self.api;
        let mut chain: Vec<Vec<u8>> = Vec::new();
        let peer = unsafe { (api.SSL_get1_peer_certificate)(self.ssl) };
        if !peer.is_null() {
            let leaf = api.der_of(peer);
            unsafe { (api.X509_free)(peer) };
            chain.push(leaf);
        }
        let stack = unsafe { (api.SSL_get_peer_cert_chain)(self.ssl) };
        if !stack.is_null() {
            for index in 0..unsafe { (api.OPENSSL_sk_num)(stack) } {
                let cert = unsafe { (api.OPENSSL_sk_value)(stack, index) };
                let der = api.der_of(cert);
                if !chain.iter().any(|existing| *existing == der) {
                    chain.push(der);
                }
            }
        }
        chain
    }

    pub fn own_certificate(&self) -> Option<Vec<u8>> {
        let cert = unsafe { (self.api.SSL_get_certificate)(self.ssl) };
        if cert.is_null() {
            None
        } else {
            Some(self.api.der_of(cert))
        }
    }

    pub fn set_sni_context(&mut self, context: &Context) -> Result<(), EngineError> {
        let api = self.api;
        let ctx = context.live()?;
        unsafe {
            (api.ERR_clear_error)();
            let cert = (api.SSL_CTX_get0_certificate)(ctx);
            let key = (api.SSL_CTX_get0_privatekey)(ctx);
            let mut chain: Ptr = std::ptr::null_mut();
            let mut ok = (api.SSL_CTX_ctrl)(ctx, SSL_CTRL_GET_CHAIN_CERTS, 0, &mut chain as *mut Ptr as Ptr) as c_int;
            if ok == 1 && !cert.is_null() {
                ok = (api.SSL_use_certificate)(self.ssl, cert);
            }
            if ok == 1 && !key.is_null() {
                ok = (api.SSL_use_PrivateKey)(self.ssl, key);
            }
            if ok == 1 && !chain.is_null() {
                ok = (api.SSL_ctrl)(self.ssl, SSL_CTRL_CHAIN, 1, chain) as c_int;
            }
            if ok != 1 {
                return Err(api.take_error("CertCbDone"));
            }
            let store = (api.SSL_CTX_get_cert_store)(ctx);
            if !store.is_null() {
                (api.SSL_ctrl)(self.ssl, 106, 1, store);
            }
            let names = (api.SSL_CTX_get_client_CA_list)(ctx);
            if !names.is_null() {
                let copy = (api.SSL_dup_CA_list)(names);
                if !copy.is_null() {
                    (api.SSL_set_client_CA_list)(self.ssl, copy);
                }
            }
        }
        Ok(())
    }

    pub fn set_max_send_fragment(&mut self, size: i64) -> bool {
        unsafe { (self.api.SSL_ctrl)(self.ssl, SSL_CTRL_SET_MAX_SEND_FRAGMENT, size as c_long, std::ptr::null_mut()) == 1 }
    }

    pub fn request_ocsp(&mut self) {
        unsafe {
            (self.api.SSL_ctrl)(self.ssl, SSL_CTRL_SET_TLSEXT_STATUS_REQ_TYPE, TLSEXT_STATUSTYPE_OCSP, std::ptr::null_mut())
        };
    }

    pub fn set_ocsp_response(&mut self, response: Vec<u8>) {
        self.state.ocsp_response = Some(response);
    }

    pub fn renegotiate(&mut self) -> Result<(), EngineError> {
        unsafe { (self.api.ERR_clear_error)() };
        if unsafe { (self.api.SSL_renegotiate)(self.ssl) } != 1 {
            return Err(self.api.take_error("SSL_renegotiate"));
        }
        Ok(())
    }

    pub fn enable_session_callbacks(&mut self) {
        self.state.session_callbacks = true;
    }

    pub fn enable_keylog(&mut self) {
        self.state.keylog = true;
    }

    pub fn enable_cert_cb(&mut self) {
        self.state.cert_cb = true;
    }

    pub fn enable_alpn_callback(&mut self) {
        self.state.alpn_callback = true;
        self.state.pause_hello = true;
    }

    pub fn enable_hello_callback(&mut self) {
        self.state.pause_hello = true;
    }

    pub fn handshake_finished(&self) -> bool {
        unsafe { (self.api.SSL_is_init_finished)(self.ssl) == 1 }
    }

    pub fn hello_pending(&self) -> bool {
        self.state.hello_reported && !self.state.hello_done
    }

    pub fn take_hello(&mut self) -> Option<ClientHello> {
        if self.state.hello_reported && !self.state.hello_done {
            self.state.hello.take()
        } else {
            None
        }
    }

    pub fn hello_done(&mut self) {
        self.state.hello_done = true;
    }

    pub fn set_alpn_choice(&mut self, offset: Option<usize>) {
        self.state.alpn_choice = offset;
    }

    pub fn take_cert_request(&mut self) -> Option<(String, bool)> {
        if self.state.cert_reported && !self.state.cert_done {
            self.state.cert_info.take()
        } else {
            None
        }
    }

    pub fn cert_done(&mut self) {
        self.state.cert_done = true;
    }

    pub fn ephemeral_key(&self) -> Option<(i32, i32)> {
        let api = self.api;
        let mut key: Ptr = std::ptr::null_mut();
        let ok = unsafe { (api.SSL_ctrl)(self.ssl, SSL_CTRL_GET_PEER_TMP_KEY, 0, &mut key as *mut Ptr as Ptr) };
        if ok != 1 || key.is_null() {
            return None;
        }
        let id = unsafe { (api.EVP_PKEY_get_base_id)(key) };
        let bits = unsafe { (api.EVP_PKEY_get_bits)(key) };
        unsafe { (api.EVP_PKEY_free)(key) };
        Some((id, bits))
    }

    pub fn shared_sigalgs(&mut self) -> Vec<String> {
        let api = self.api;
        let mut result = Vec::new();
        let mut index = 0;
        loop {
            let (mut sign, mut hash, mut sign_hash) = (0, 0, 0);
            let (mut rsig, mut rhash) = (0u8, 0u8);
            let count = unsafe {
                (api.SSL_get_shared_sigalgs)(
                    self.ssl,
                    index,
                    &mut sign,
                    &mut hash,
                    &mut sign_hash,
                    &mut rsig,
                    &mut rhash,
                )
            };
            if count <= 0 {
                break;
            }
            result.push(format!("{sign}+{hash}+{sign_hash}+{rsig}+{rhash}"));
            index += 1;
        }
        result
    }

    pub fn security_level(&self) -> i32 {
        unsafe { (self.api.SSL_get_security_level)(self.ssl) }
    }

    pub fn handshake_pending_close(&self) -> bool {
        unsafe { (self.api.SSL_get_shutdown)(self.ssl) != 0 }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        unsafe {
            if !self.state.next_session.is_null() {
                (self.api.SSL_SESSION_free)(self.state.next_session);
            }
            (self.api.SSL_set_ex_data)(self.ssl, 0, std::ptr::null_mut());
            (self.api.SSL_free)(self.ssl);
        }
        let _ = (self.rbio, self.wbio);
    }
}

// ---- OpenSSL-faithful additions ---------------------------------------------------------------
//
// What an embedder needs when it keeps OpenSSL's own semantics (the verification verdict aborts
// the handshake, the trust store follows `SSL_CTX_load_verify_locations`, a handshake is driven
// explicitly): Python's `_ssl` is built on these. Node's `tls` uses the surface above.

const ERR_LIB_X509: c_ulong = 11;
const ERR_LIB_ASN1: c_ulong = 13;
const X509_R_CERT_ALREADY_IN_HASH_TABLE: c_ulong = 101;
const ASN1_R_HEADER_TOO_LONG: c_ulong = 123;
const X509_LU_CRL: c_int = 2;
const SSL_CTRL_SET_MSG_CALLBACK: c_int = 15;
const SSL_CTRL_SESS_NUMBER: c_int = 20;
const BIO_C_SET_BUF_MEM_EOF_RETURN: c_int = 130;
const SSL_VERIFY_POST_HANDSHAKE: c_int = 8;
const SSL_RECEIVED_SHUTDOWN: c_int = 2;

/// What OpenSSL calls an error code: its library (`SSL`), the symbolic reason
/// (`CERTIFICATE_VERIFY_FAILED`) and the reason text (`certificate verify failed`).
#[derive(Debug, Clone, Default)]
pub struct ErrorDetails {
    pub library: Option<String>,
    pub reason: Option<String>,
    pub text: Option<String>,
}

/// The library, symbolic reason and text of a raw OpenSSL error code.
pub fn error_details(code: u64) -> ErrorDetails {
    let Ok(api) = api() else { return ErrorDetails::default() };
    if code == 0 {
        return ErrorDetails::default();
    }
    let code = code as c_ulong;
    let library = match lib_prefix(err_lib(code)) {
        "" => None,
        prefix => Some(prefix.trim_end_matches('_').to_string()),
    };
    let text = cstr(unsafe { (api.ERR_reason_error_string)(code) });
    let reason = text.as_ref().map(|text| {
        text.chars()
            .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' })
            .collect()
    });
    ErrorDetails { library, reason, text }
}

/// `ERR_GET_LIB` and `ERR_GET_REASON` of a raw OpenSSL error code.
pub fn error_parts(code: u64) -> (u64, u64) {
    (err_lib(code as c_ulong) as u64, err_reason(code as c_ulong) as u64)
}

/// The text of an X509 verification result code (`X509_verify_cert_error_string`).
pub fn verify_error_text(code: i64) -> Option<String> {
    let api = api().ok()?;
    cstr(unsafe { (api.X509_verify_cert_error_string)(code as c_long) })
}

/// `(OPENSSL_VERSION_NUMBER, "OpenSSL 3.0.13 30 Jan 2024")` of the loaded library.
pub fn openssl_version() -> Option<(u64, String)> {
    let api = api().ok()?;
    let number = unsafe { (api.OpenSSL_version_num)() } as u64;
    let text = cstr(unsafe { (api.OpenSSL_version)(0) })?;
    Some((number, text))
}

/// `(cafile environment variable, cafile, capath environment variable, capath)`: where OpenSSL
/// looks for trusted certificates by default.
pub fn default_verify_paths() -> Option<(String, String, String, String)> {
    let api = api().ok()?;
    unsafe {
        Some((
            cstr((api.X509_get_default_cert_file_env)())?,
            cstr((api.X509_get_default_cert_file)())?,
            cstr((api.X509_get_default_cert_dir_env)())?,
            cstr((api.X509_get_default_cert_dir)())?,
        ))
    }
}

/// An ASN.1 object identifier with the names OpenSSL knows it by.
#[derive(Debug, Clone)]
pub struct ObjectInfo {
    pub nid: i32,
    pub short_name: String,
    pub long_name: String,
    pub oid: String,
}

fn object_info(api: &Api, object: CPtr) -> Option<ObjectInfo> {
    let nid = unsafe { (api.OBJ_obj2nid)(object) };
    if nid == 0 {
        return None;
    }
    let mut buffer = [0 as c_char; 256];
    let written = unsafe { (api.OBJ_obj2txt)(buffer.as_mut_ptr(), buffer.len() as c_int, object, 1) };
    if written < 0 {
        return None;
    }
    Some(ObjectInfo {
        nid,
        short_name: cstr(unsafe { (api.OBJ_nid2sn)(nid) }).unwrap_or_default(),
        long_name: cstr(unsafe { (api.OBJ_nid2ln)(nid) }).unwrap_or_default(),
        oid: cstr(buffer.as_ptr()).unwrap_or_default(),
    })
}

/// `OBJ_txt2obj`: an object by dotted OID or, with `by_name`, by short or long name.
pub fn object_from_text(text: &str, by_name: bool) -> Option<ObjectInfo> {
    let api = api().ok()?;
    let text = CString::new(text).ok()?;
    let object = unsafe { (api.OBJ_txt2obj)(text.as_ptr(), if by_name { 0 } else { 1 }) };
    unsafe { (api.ERR_clear_error)() };
    if object.is_null() {
        return None;
    }
    let info = object_info(api, object);
    unsafe { (api.ASN1_OBJECT_free)(object) };
    info
}

/// `OBJ_nid2obj`.
pub fn object_from_nid(nid: i32) -> Option<ObjectInfo> {
    let api = api().ok()?;
    let object = unsafe { (api.OBJ_nid2obj)(nid) };
    unsafe { (api.ERR_clear_error)() };
    if object.is_null() {
        return None;
    }
    object_info(api, object)
}

/// A decoded `SSL_SESSION`.
#[derive(Debug, Clone, Default)]
pub struct SessionInfo {
    pub id: Vec<u8>,
    pub time: i64,
    pub timeout: i64,
    pub has_ticket: bool,
    pub ticket_lifetime_hint: u64,
}

/// The fields of a session serialized by [`Session::session_bytes`].
pub fn session_info(der: &[u8]) -> Option<SessionInfo> {
    let api = api().ok()?;
    let mut cursor = der.as_ptr();
    let session = unsafe { (api.d2i_SSL_SESSION)(std::ptr::null_mut(), &mut cursor, der.len() as c_long) };
    if session.is_null() {
        unsafe { (api.ERR_clear_error)() };
        return None;
    }
    let mut id_length: c_uint = 0;
    let id = unsafe { (api.SSL_SESSION_get_id)(session, &mut id_length) };
    let info = SessionInfo {
        id: if id.is_null() {
            Vec::new()
        } else {
            unsafe { std::slice::from_raw_parts(id, id_length as usize) }.to_vec()
        },
        time: unsafe { (api.SSL_SESSION_get_time)(session) } as i64,
        timeout: unsafe { (api.SSL_SESSION_get_timeout)(session) } as i64,
        has_ticket: unsafe { (api.SSL_SESSION_has_ticket)(session) } != 0,
        ticket_lifetime_hint: unsafe { (api.SSL_SESSION_get_ticket_lifetime_hint)(session) } as u64,
    };
    unsafe { (api.SSL_SESSION_free)(session) };
    Some(info)
}

/// One cipher suite as `SSLContext.get_ciphers()` describes it.
#[derive(Debug, Clone, Default)]
pub struct CipherInfo {
    pub id: u32,
    pub name: String,
    pub protocol: String,
    pub description: String,
    pub strength_bits: i32,
    pub alg_bits: i32,
    pub aead: bool,
    pub symmetric: Option<String>,
    pub digest: Option<String>,
    pub kea: Option<String>,
    pub auth: Option<String>,
}

fn cipher_info(api: &Api, cipher: CPtr) -> CipherInfo {
    let mut alg_bits: c_int = 0;
    let strength_bits = unsafe { (api.SSL_CIPHER_get_bits)(cipher, &mut alg_bits) };
    let mut description = [0 as c_char; 512];
    unsafe { (api.SSL_CIPHER_description)(cipher, description.as_mut_ptr(), description.len() as c_int) };
    let named = |nid: c_int| {
        if nid == 0 {
            None
        } else {
            cstr(unsafe { (api.OBJ_nid2ln)(nid) })
        }
    };
    CipherInfo {
        id: unsafe { (api.SSL_CIPHER_get_id)(cipher) },
        name: cstr(unsafe { (api.SSL_CIPHER_get_name)(cipher) }).unwrap_or_default(),
        protocol: cstr(unsafe { (api.SSL_CIPHER_get_version)(cipher) }).unwrap_or_default(),
        description: cstr(description.as_ptr()).unwrap_or_default(),
        strength_bits,
        alg_bits,
        aead: unsafe { (api.SSL_CIPHER_is_aead)(cipher) } != 0,
        symmetric: named(unsafe { (api.SSL_CIPHER_get_cipher_nid)(cipher) }),
        digest: named(unsafe { (api.SSL_CIPHER_get_digest_nid)(cipher) }),
        kea: named(unsafe { (api.SSL_CIPHER_get_kx_nid)(cipher) }),
        auth: named(unsafe { (api.SSL_CIPHER_get_auth_nid)(cipher) }),
    }
}

/// How OpenSSL asks for the passphrase of an encrypted private key.
struct Passphrase<'a> {
    callback: Option<&'a mut dyn FnMut(usize) -> Result<Vec<u8>, ()>>,
}

unsafe extern "C" fn passphrase_callback(buffer: *mut c_char, size: c_int, _write: c_int, user: Ptr) -> c_int {
    let state = unsafe { &mut *(user as *mut Passphrase) };
    let Some(callback) = state.callback.as_mut() else { return -1 };
    match callback(size.max(0) as usize) {
        Ok(bytes) => {
            let length = bytes.len().min(size.max(0) as usize);
            unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer as *mut u8, length) };
            length as c_int
        }
        Err(()) => -1,
    }
}

unsafe extern "C" fn message_callback(
    write: c_int,
    version: c_int,
    content_type: c_int,
    buffer: CPtr,
    length: usize,
    ssl: Ptr,
    _arg: Ptr,
) {
    let api = global_api();
    let Some(state) = state_of(api, ssl) else { return };
    let data = if buffer.is_null() || length == 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(buffer as *const u8, length) }.to_vec()
    };
    state.messages.push(Message {
        write: write == 1,
        version,
        content_type,
        data,
    });
}

impl Context {
    pub fn clear_options(&mut self, options: u64) -> Result<(), EngineError> {
        let ctx = self.live()?;
        unsafe { (self.api.SSL_CTX_clear_options)(ctx, options) };
        Ok(())
    }

    pub fn options(&self) -> u64 {
        if self.ctx.is_null() {
            return 0;
        }
        unsafe { (self.api.SSL_CTX_get_options)(self.ctx) }
    }

    /// `SSL_CTX_set_verify` with OpenSSL's default verify callback.
    pub fn set_verify(&mut self, mode: i32) -> Result<(), EngineError> {
        let ctx = self.live()?;
        unsafe { (self.api.SSL_CTX_set_verify)(ctx, mode, None) };
        Ok(())
    }

    pub fn verify_mode(&self) -> i32 {
        if self.ctx.is_null() {
            return 0;
        }
        unsafe { (self.api.SSL_CTX_get_verify_mode)(self.ctx) }
    }

    pub fn verify_flags(&self) -> u64 {
        if self.ctx.is_null() {
            return 0;
        }
        let param = unsafe { (self.api.SSL_CTX_get0_param)(self.ctx) };
        unsafe { (self.api.X509_VERIFY_PARAM_get_flags)(param) as u64 }
    }

    /// Makes the context's X509 verification flags exactly `flags`.
    pub fn set_verify_flags(&mut self, flags: u64) -> Result<(), EngineError> {
        let api = self.api;
        let ctx = self.live()?;
        let param = unsafe { (api.SSL_CTX_get0_param)(ctx) };
        let current = unsafe { (api.X509_VERIFY_PARAM_get_flags)(param) } as u64;
        let clear = current & !flags;
        let set = !current & flags;
        unsafe { (api.ERR_clear_error)() };
        if clear != 0 && unsafe { (api.X509_VERIFY_PARAM_clear_flags)(param, clear as c_ulong) } != 1 {
            return Err(api.take_error("X509_VERIFY_PARAM_clear_flags"));
        }
        if set != 0 && unsafe { (api.X509_VERIFY_PARAM_set_flags)(param, set as c_ulong) } != 1 {
            return Err(api.take_error("X509_VERIFY_PARAM_set_flags"));
        }
        Ok(())
    }

    /// `X509_CHECK_FLAG_*` applied to every hostname check of the context's sessions.
    pub fn set_hostflags(&mut self, flags: u32) -> Result<(), EngineError> {
        let ctx = self.live()?;
        let param = unsafe { (self.api.SSL_CTX_get0_param)(ctx) };
        unsafe { (self.api.X509_VERIFY_PARAM_set_hostflags)(param, flags as c_uint) };
        Ok(())
    }

    pub fn security_level(&self) -> i32 {
        if self.ctx.is_null() {
            return 0;
        }
        unsafe { (self.api.SSL_CTX_get_security_level)(self.ctx) }
    }

    pub fn set_session_cache_mode(&mut self, mode: i32) -> Result<(), EngineError> {
        let ctx = self.live()?;
        unsafe { (self.api.SSL_CTX_ctrl)(ctx, SSL_CTRL_SET_SESS_CACHE_MODE, mode as c_long, std::ptr::null_mut()) };
        Ok(())
    }

    /// `number, connect, connect_good, connect_renegotiate, accept, accept_good,
    /// accept_renegotiate, hits, cb_hits, misses, timeouts, cache_full`.
    pub fn session_stats(&self) -> [i64; 12] {
        let mut stats = [0i64; 12];
        if self.ctx.is_null() {
            return stats;
        }
        for (index, slot) in stats.iter_mut().enumerate() {
            *slot = unsafe {
                (self.api.SSL_CTX_ctrl)(self.ctx, SSL_CTRL_SESS_NUMBER + index as c_int, 0, std::ptr::null_mut())
            } as i64;
        }
        stats
    }

    pub fn num_tickets(&self) -> usize {
        if self.ctx.is_null() {
            return 0;
        }
        unsafe { (self.api.SSL_CTX_get_num_tickets)(self.ctx) }
    }

    pub fn set_num_tickets(&mut self, count: usize) -> bool {
        match self.live() {
            Ok(ctx) => unsafe { (self.api.SSL_CTX_set_num_tickets)(ctx, count) == 1 },
            Err(_) => false,
        }
    }

    /// `SSL_CTX_load_verify_locations`: a CA bundle file and/or a hashed certificate directory.
    pub fn load_verify_locations(&mut self, file: Option<&str>, dir: Option<&str>) -> Result<(), EngineError> {
        let api = self.api;
        let ctx = self.live()?;
        let to_c = |text: Option<&str>| text.map(CString::new).transpose().map_err(|_| EngineError::plain("embedded null byte"));
        let file = to_c(file)?;
        let dir = to_c(dir)?;
        self.private_store()?;
        unsafe { (api.ERR_clear_error)() };
        let ok = unsafe {
            (api.SSL_CTX_load_verify_locations)(
                ctx,
                file.as_ref().map_or(std::ptr::null(), |text| text.as_ptr()),
                dir.as_ref().map_or(std::ptr::null(), |text| text.as_ptr()),
            )
        };
        if ok != 1 {
            return Err(api.take_error("SSL_CTX_load_verify_locations"));
        }
        Ok(())
    }

    /// `SSL_CTX_set_default_verify_paths`; where OpenSSL's defaults name neither an existing file
    /// nor directory, the platform's own root certificates are loaded instead.
    pub fn set_default_verify_paths(&mut self) -> Result<(), EngineError> {
        let api = self.api;
        let ctx = self.live()?;
        let store = self.private_store()?;
        unsafe { (api.ERR_clear_error)() };
        if unsafe { (api.SSL_CTX_set_default_verify_paths)(ctx) } != 1 {
            return Err(api.take_error("SSL_CTX_set_default_verify_paths"));
        }
        if let Some((file_env, file, dir_env, dir)) = default_verify_paths() {
            let file = std::env::var(file_env).unwrap_or(file);
            let dir = std::env::var(dir_env).unwrap_or(dir);
            if !std::path::Path::new(&file).is_file() && !std::path::Path::new(&dir).is_dir() {
                for pem in root_certificate_pems() {
                    let _ = load_pem_text(api, store, pem.as_bytes());
                }
            }
        }
        Ok(())
    }

    /// Adds every certificate of `data` (concatenated PEM, or concatenated DER) to the trust
    /// store; `cadata` of `SSLContext.load_verify_locations`.
    pub fn add_ca_data(&mut self, data: &[u8], der: bool) -> Result<(), EngineError> {
        let api = self.api;
        self.live()?;
        let store = self.private_store()?;
        let bio = api.mem_bio(data)?;
        unsafe { (api.ERR_clear_error)() };
        let mut loaded = 0usize;
        let mut failure: Option<EngineError> = None;
        loop {
            let cert = unsafe {
                if der {
                    (api.d2i_X509_bio)(bio, std::ptr::null_mut())
                } else {
                    (api.PEM_read_bio_X509)(bio, std::ptr::null_mut(), std::ptr::null(), std::ptr::null_mut())
                }
            };
            if cert.is_null() {
                break;
            }
            let added = unsafe { (api.X509_STORE_add_cert)(store, cert) };
            unsafe { (api.X509_free)(cert) };
            if added != 1 {
                let err = unsafe { (api.ERR_peek_last_error)() };
                if err_lib(err) == ERR_LIB_X509 && err_reason(err) == X509_R_CERT_ALREADY_IN_HASH_TABLE {
                    unsafe { (api.ERR_clear_error)() };
                } else {
                    failure = Some(api.take_error("X509_STORE_add_cert"));
                    break;
                }
            }
            loaded += 1;
        }
        unsafe { (api.BIO_free)(bio) };
        if let Some(error) = failure {
            return Err(error);
        }
        let err = unsafe { (api.ERR_peek_last_error)() };
        let end_of_data = if der {
            err_lib(err) == ERR_LIB_ASN1 && err_reason(err) == ASN1_R_HEADER_TOO_LONG
        } else {
            err_lib(err) == ERR_LIB_PEM && err_reason(err) == PEM_R_NO_START_LINE
        };
        if end_of_data {
            unsafe { (api.ERR_clear_error)() };
        } else if err != 0 {
            return Err(api.take_error("add_ca_data"));
        }
        if loaded == 0 {
            return Err(EngineError::plain(if der {
                "not enough data: cadata does not contain a certificate"
            } else {
                "no start line: cadata does not contain a certificate"
            }));
        }
        Ok(())
    }

    fn store_objects(&self) -> Vec<(c_int, Ptr)> {
        let api = self.api;
        if self.ctx.is_null() {
            return Vec::new();
        }
        let store = unsafe { (api.SSL_CTX_get_cert_store)(self.ctx) };
        if store.is_null() {
            return Vec::new();
        }
        let objects = unsafe { (api.X509_STORE_get0_objects)(store) };
        if objects.is_null() {
            return Vec::new();
        }
        (0..unsafe { (api.OPENSSL_sk_num)(objects) })
            .map(|index| {
                let object = unsafe { (api.OPENSSL_sk_value)(objects, index) };
                (unsafe { (api.X509_OBJECT_get_type)(object) }, object)
            })
            .collect()
    }

    /// The DER of every CA certificate in the trust store (`get_ca_certs`).
    pub fn ca_certs(&self) -> Vec<Vec<u8>> {
        let api = self.api;
        self.store_objects()
            .into_iter()
            .filter(|(kind, _)| *kind == X509_LU_X509)
            .filter_map(|(_, object)| {
                let cert = unsafe { (api.X509_OBJECT_get0_X509)(object) };
                (unsafe { (api.X509_check_ca)(cert) } != 0).then(|| api.der_of(cert))
            })
            .collect()
    }

    /// `(certificates, revocation lists, CA certificates)` in the trust store.
    pub fn store_stats(&self) -> (usize, usize, usize) {
        let api = self.api;
        let (mut x509, mut crl, mut ca) = (0, 0, 0);
        for (kind, object) in self.store_objects() {
            if kind == X509_LU_X509 {
                x509 += 1;
                let cert = unsafe { (api.X509_OBJECT_get0_X509)(object) };
                if unsafe { (api.X509_check_ca)(cert) } != 0 {
                    ca += 1;
                }
            } else if kind == X509_LU_CRL {
                crl += 1;
            }
        }
        (x509, crl, ca)
    }

    /// Loads the private key of `pem`. An encrypted key asks `password` (called with the
    /// buffer size OpenSSL offers); without one, loading an encrypted key fails instead of
    /// prompting a terminal.
    pub fn use_private_key(
        &mut self,
        pem: &[u8],
        password: Option<&mut dyn FnMut(usize) -> Result<Vec<u8>, ()>>,
    ) -> Result<(), EngineError> {
        let api = self.api;
        let ctx = self.live()?;
        let bio = api.mem_bio(pem)?;
        unsafe { (api.ERR_clear_error)() };
        let mut state = Passphrase { callback: password };
        let key = unsafe {
            (api.PEM_read_bio_PrivateKey)(
                bio,
                std::ptr::null_mut(),
                passphrase_callback as unsafe extern "C" fn(*mut c_char, c_int, c_int, Ptr) -> c_int as *const c_void,
                &mut state as *mut Passphrase as Ptr,
            )
        };
        unsafe { (api.BIO_free)(bio) };
        if key.is_null() {
            return Err(api.take_error("PEM_read_bio_PrivateKey"));
        }
        let ok = unsafe { (api.SSL_CTX_use_PrivateKey)(ctx, key) };
        unsafe { (api.EVP_PKEY_free)(key) };
        if ok != 1 {
            return Err(api.take_error("SSL_CTX_use_PrivateKey"));
        }
        Ok(())
    }

    /// `SSL_CTX_check_private_key` with OpenSSL's error on a mismatch.
    pub fn verify_private_key(&self) -> Result<(), EngineError> {
        let ctx = self.live()?;
        unsafe { (self.api.ERR_clear_error)() };
        if unsafe { (self.api.SSL_CTX_check_private_key)(ctx) } != 1 {
            return Err(self.api.take_error("SSL_CTX_check_private_key"));
        }
        Ok(())
    }

    /// Diffie-Hellman parameters from `pem` (`load_dh_params`).
    pub fn load_dh_params(&mut self, pem: &[u8]) -> Result<(), EngineError> {
        let api = self.api;
        let ctx = self.live()?;
        let bio = api.mem_bio(pem)?;
        unsafe { (api.ERR_clear_error)() };
        let dh = unsafe {
            (api.PEM_read_bio_DHparams)(bio, std::ptr::null_mut(), std::ptr::null(), std::ptr::null_mut())
        };
        unsafe { (api.BIO_free)(bio) };
        if dh.is_null() {
            return Err(api.take_error("PEM_read_bio_DHparams"));
        }
        let ok = unsafe { (api.SSL_CTX_ctrl)(ctx, SSL_CTRL_SET_TMP_DH, 0, dh) };
        unsafe { (api.DH_free)(dh) };
        if ok != 1 {
            return Err(api.take_error("SSL_CTX_set_tmp_dh"));
        }
        Ok(())
    }

    /// The cipher suites a session of this context offers, in preference order.
    pub fn ciphers(&self) -> Result<Vec<CipherInfo>, EngineError> {
        let api = self.api;
        let ctx = self.live()?;
        let ssl = unsafe { (api.SSL_new)(ctx) };
        if ssl.is_null() {
            return Err(api.take_error("SSL_new"));
        }
        let mut out = Vec::new();
        let stack = unsafe { (api.SSL_get_ciphers)(ssl) };
        if !stack.is_null() {
            for index in 0..unsafe { (api.OPENSSL_sk_num)(stack) } {
                out.push(cipher_info(api, unsafe { (api.OPENSSL_sk_value)(stack, index) }));
            }
        }
        unsafe { (api.SSL_free)(ssl) };
        Ok(out)
    }
}

impl Session {
    /// Runs the handshake as far as the buffered bytes allow: `0` when it is complete, else the
    /// `SSL_ERROR_*` code (`SSL_ERROR_WANT_READ` when more bytes must be fed).
    pub fn do_handshake(&mut self) -> i32 {
        unsafe {
            (self.api.ERR_clear_error)();
            let ret = (self.api.SSL_do_handshake)(self.ssl);
            if ret == 1 {
                0
            } else {
                (self.api.SSL_get_error)(self.ssl, ret)
            }
        }
    }

    /// One `SSL_shutdown`: `(return value, SSL_ERROR_* code or 0)`.
    pub fn shutdown_step(&mut self) -> (i32, i32) {
        unsafe {
            (self.api.ERR_clear_error)();
            let ret = (self.api.SSL_shutdown)(self.ssl);
            let err = if ret < 0 { (self.api.SSL_get_error)(self.ssl, ret) } else { 0 };
            (ret, err)
        }
    }

    /// Decrypted bytes already buffered by the library (`SSL_pending`).
    pub fn pending(&self) -> usize {
        unsafe { (self.api.SSL_pending)(self.ssl) }.max(0) as usize
    }

    /// Whether the peer's close_notify was received.
    pub fn received_shutdown(&self) -> bool {
        unsafe { (self.api.SSL_get_shutdown)(self.ssl) & SSL_RECEIVED_SHUTDOWN != 0 }
    }

    /// Signals end of input: once the fed bytes are consumed, reads see EOF instead of asking
    /// for more.
    pub fn feed_eof(&mut self) {
        unsafe { (self.api.BIO_ctrl)(self.rbio, BIO_C_SET_BUF_MEM_EOF_RETURN, 0, std::ptr::null_mut()) };
    }

    /// The newest raw code on OpenSSL's error queue, without consuming it.
    pub fn peek_error(&self) -> u64 {
        unsafe { (self.api.ERR_peek_last_error)() as u64 }
    }

    /// The raw X509 verification result (`X509_V_OK` is 0).
    pub fn verify_result(&self) -> i64 {
        unsafe { (self.api.SSL_get_verify_result)(self.ssl) as i64 }
    }

    pub fn verify_mode(&self) -> i32 {
        unsafe { (self.api.SSL_get_verify_mode)(self.ssl) }
    }

    /// Checks the peer certificate against `host` (an IP address when `ip`) with `hostflags`.
    pub fn set_host_check(&mut self, host: &str, hostflags: u32, ip: bool) -> Result<(), EngineError> {
        let api = self.api;
        let host = CString::new(host).map_err(|_| EngineError::plain("embedded null byte"))?;
        unsafe { (api.ERR_clear_error)() };
        let param = unsafe { (api.SSL_get0_param)(self.ssl) };
        unsafe { (api.X509_VERIFY_PARAM_set_hostflags)(param, hostflags as c_uint) };
        let ok = unsafe {
            if ip {
                (api.X509_VERIFY_PARAM_set1_ip_asc)(param, host.as_ptr())
            } else {
                (api.X509_VERIFY_PARAM_set1_host)(param, host.as_ptr(), 0)
            }
        };
        if ok != 1 {
            return Err(api.take_error("X509_VERIFY_PARAM_set1_host"));
        }
        Ok(())
    }

    /// Enables TLS 1.3 post-handshake authentication: a client offers it, a server may later
    /// request a certificate with [`Session::verify_client_post_handshake`].
    pub fn enable_post_handshake_auth(&mut self) {
        unsafe {
            if self.is_server {
                let mode = (self.api.SSL_get_verify_mode)(self.ssl);
                (self.api.SSL_set_verify)(self.ssl, mode | SSL_VERIFY_POST_HANDSHAKE, None);
            } else {
                (self.api.SSL_set_post_handshake_auth)(self.ssl, 1);
            }
        }
    }

    pub fn verify_client_post_handshake(&mut self) -> Result<(), EngineError> {
        unsafe { (self.api.ERR_clear_error)() };
        if unsafe { (self.api.SSL_verify_client_post_handshake)(self.ssl) } != 1 {
            return Err(self.api.take_error("SSL_verify_client_post_handshake"));
        }
        Ok(())
    }

    /// Continues this session with `context`'s certificates and settings (`SSL_set_SSL_CTX`).
    pub fn switch_context(&mut self, context: &Context) -> Result<(), EngineError> {
        let ctx = context.live()?;
        unsafe { (self.api.SSL_set_SSL_CTX)(self.ssl, ctx) };
        Ok(())
    }

    /// Secret key bits of the negotiated cipher.
    pub fn cipher_bits(&self) -> i32 {
        let cipher = unsafe { (self.api.SSL_get_current_cipher)(self.ssl) };
        if cipher.is_null() {
            return 0;
        }
        unsafe { (self.api.SSL_CIPHER_get_bits)(cipher, std::ptr::null_mut()) }
    }

    /// `(name, protocol, bits)` of the server's ciphers the client also offered.
    pub fn shared_ciphers(&self) -> Option<Vec<(String, String, i32)>> {
        let api = self.api;
        let server = unsafe { (api.SSL_get_ciphers)(self.ssl) };
        if server.is_null() {
            return None;
        }
        let client = unsafe { (api.SSL_get_client_ciphers)(self.ssl) };
        if client.is_null() {
            return None;
        }
        let offered: Vec<Ptr> = (0..unsafe { (api.OPENSSL_sk_num)(client) })
            .map(|index| unsafe { (api.OPENSSL_sk_value)(client, index) })
            .collect();
        let mut out = Vec::new();
        for index in 0..unsafe { (api.OPENSSL_sk_num)(server) } {
            let cipher = unsafe { (api.OPENSSL_sk_value)(server, index) };
            if !offered.contains(&cipher) {
                continue;
            }
            out.push((
                cstr(unsafe { (api.SSL_CIPHER_get_name)(cipher) }).unwrap_or_default(),
                cstr(unsafe { (api.SSL_CIPHER_get_version)(cipher) }).unwrap_or_default(),
                unsafe { (api.SSL_CIPHER_get_bits)(cipher, std::ptr::null_mut()) },
            ));
        }
        Some(out)
    }

    /// The negotiated compression method's name, if compression is in use.
    pub fn compression(&self) -> Option<String> {
        let api = self.api;
        let method = unsafe { (api.SSL_get_current_compression)(self.ssl) };
        if method.is_null() {
            return None;
        }
        let nid = unsafe { (api.COMP_get_type)(method) };
        if nid == 0 {
            return None;
        }
        cstr(unsafe { (api.OBJ_nid2sn)(nid) })
    }

    /// Starts recording protocol messages for [`Session::take_messages`].
    pub fn enable_messages(&mut self) {
        let callback: unsafe extern "C" fn(c_int, c_int, c_int, CPtr, usize, Ptr, Ptr) = message_callback;
        // SAFETY: OpenSSL stores the pointer as `void (*)(void)` and casts it back to the
        // message callback signature before calling it.
        let callback: unsafe extern "C" fn() = unsafe { std::mem::transmute(callback) };
        unsafe { (self.api.SSL_callback_ctrl)(self.ssl, SSL_CTRL_SET_MSG_CALLBACK, Some(callback)) };
    }

    pub fn take_messages(&mut self) -> Vec<Message> {
        std::mem::take(&mut self.state.messages)
    }

    /// Fails a paused client hello with TLS alert `alert` instead of resuming it.
    pub fn set_hello_alert(&mut self, alert: i32) {
        self.state.hello_alert = Some(alert);
        self.state.hello_done = true;
    }

    /// Make a server that finds no common ALPN protocol continue without one, rather than send
    /// a `no_application_protocol` alert.
    pub fn set_alpn_lenient(&mut self) {
        self.state.alpn_noack = true;
    }
}

impl Context {
    /// `SSL_CTX_set_min_proto_version` / `SSL_CTX_set_max_proto_version`: whether OpenSSL accepted
    /// `version` (`0` lifts the bound).
    pub fn try_set_proto(&mut self, max: bool, version: i32) -> bool {
        let Ok(ctx) = self.live() else { return false };
        let cmd = if max { SSL_CTRL_SET_MAX_PROTO_VERSION } else { SSL_CTRL_SET_MIN_PROTO_VERSION };
        unsafe { (self.api.SSL_CTX_ctrl)(ctx, cmd, version as c_long, std::ptr::null_mut()) == 1 }
    }
}

/// Whether OpenSSL knows an object with the short name `name` (an elliptic curve name).
pub fn short_name_known(name: &str) -> bool {
    let (Ok(api), Ok(name)) = (api(), CString::new(name)) else { return false };
    unsafe { (api.OBJ_sn2nid)(name.as_ptr()) != 0 }
}
