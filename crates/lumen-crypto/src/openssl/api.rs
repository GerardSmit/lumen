//! The OpenSSL 3 symbols this backend calls, resolved from `libcrypto` through the shared loader.

use std::os::raw::{c_char, c_int, c_uint, c_ulong, c_void};

use lumen_os::dynlib::Library;

pub(super) type Ptr = *mut c_void;
pub(super) type CPtr = *const c_void;
pub(super) type Free = unsafe extern "C" fn(Ptr);
/// `EVP_PKEY_encrypt` / `decrypt` / `sign` / `verify_recover` all share this shape.
pub(super) type CryptOp = unsafe extern "C" fn(Ptr, *mut u8, *mut usize, *const u8, usize) -> c_int;

macro_rules! api {
    ($( $name:ident ( $($arg:ty),* ) $(-> $ret:ty)? ;)*) => {
        #[allow(non_snake_case)]
        pub(super) struct Api {
            $( pub $name: unsafe extern "C" fn($($arg),*) $(-> $ret)?, )*
        }
        impl Api {
            pub(super) fn load(lib: &Library) -> Result<Api, String> {
                unsafe {
                    Ok(Api { $( $name: lib.function(stringify!($name))?, )* })
                }
            }
        }
    };
}

api! {
    OPENSSL_version_major() -> c_uint;
    ERR_get_error() -> c_ulong;
    ERR_clear_error();
    ERR_lib_error_string(c_ulong) -> *const c_char;
    ERR_reason_error_string(c_ulong) -> *const c_char;
    CRYPTO_malloc(usize, *const c_char, c_int) -> Ptr;
    CRYPTO_free(Ptr, *const c_char, c_int);

    BN_new() -> Ptr;
    BN_free(Ptr);
    BN_bin2bn(*const u8, c_int, Ptr) -> Ptr;
    BN_bn2bin(CPtr, *mut u8) -> c_int;
    BN_num_bits(CPtr) -> c_int;
    BN_set_flags(Ptr, c_int);
    BN_CTX_new() -> Ptr;
    BN_CTX_free(Ptr);
    BN_mod_exp(Ptr, CPtr, CPtr, CPtr, Ptr) -> c_int;
    BN_generate_prime_ex(Ptr, c_int, c_int, CPtr, CPtr, Ptr) -> c_int;
    BN_is_prime_ex(CPtr, c_int, Ptr, Ptr) -> c_int;

    OSSL_PARAM_BLD_new() -> Ptr;
    OSSL_PARAM_BLD_free(Ptr);
    OSSL_PARAM_BLD_push_BN(Ptr, *const c_char, CPtr) -> c_int;
    OSSL_PARAM_BLD_to_param(Ptr) -> Ptr;
    OSSL_PARAM_free(Ptr);

    EVP_get_digestbyname(*const c_char) -> CPtr;
    EVP_PKEY_free(Ptr);
    EVP_PKEY_CTX_new(Ptr, Ptr) -> Ptr;
    EVP_PKEY_CTX_new_from_name(Ptr, *const c_char, *const c_char) -> Ptr;
    EVP_PKEY_CTX_free(Ptr);
    EVP_PKEY_fromdata_init(Ptr) -> c_int;
    EVP_PKEY_fromdata(Ptr, *mut Ptr, c_int, Ptr) -> c_int;
    EVP_PKEY_get_bn_param(CPtr, *const c_char, *mut Ptr) -> c_int;

    EVP_PKEY_encrypt_init(Ptr) -> c_int;
    EVP_PKEY_encrypt(Ptr, *mut u8, *mut usize, *const u8, usize) -> c_int;
    EVP_PKEY_decrypt_init(Ptr) -> c_int;
    EVP_PKEY_decrypt(Ptr, *mut u8, *mut usize, *const u8, usize) -> c_int;
    EVP_PKEY_sign_init(Ptr) -> c_int;
    EVP_PKEY_sign(Ptr, *mut u8, *mut usize, *const u8, usize) -> c_int;
    EVP_PKEY_verify_init(Ptr) -> c_int;
    EVP_PKEY_verify(Ptr, *const u8, usize, *const u8, usize) -> c_int;
    EVP_PKEY_verify_recover_init(Ptr) -> c_int;
    EVP_PKEY_verify_recover(Ptr, *mut u8, *mut usize, *const u8, usize) -> c_int;
    EVP_PKEY_keygen_init(Ptr) -> c_int;
    EVP_PKEY_keygen(Ptr, *mut Ptr) -> c_int;
    EVP_PKEY_paramgen_init(Ptr) -> c_int;
    EVP_PKEY_paramgen(Ptr, *mut Ptr) -> c_int;

    EVP_PKEY_CTX_set_rsa_padding(Ptr, c_int) -> c_int;
    EVP_PKEY_CTX_set_signature_md(Ptr, CPtr) -> c_int;
    EVP_PKEY_CTX_set_rsa_pss_saltlen(Ptr, c_int) -> c_int;
    EVP_PKEY_CTX_set_rsa_mgf1_md(Ptr, CPtr) -> c_int;
    EVP_PKEY_CTX_set_rsa_oaep_md(Ptr, CPtr) -> c_int;
    EVP_PKEY_CTX_set0_rsa_oaep_label(Ptr, Ptr, c_int) -> c_int;
    EVP_PKEY_CTX_set_rsa_keygen_bits(Ptr, c_int) -> c_int;
    EVP_PKEY_CTX_set1_rsa_keygen_pubexp(Ptr, CPtr) -> c_int;
    EVP_PKEY_CTX_set_dsa_paramgen_bits(Ptr, c_int) -> c_int;
    EVP_PKEY_CTX_set_dsa_paramgen_q_bits(Ptr, c_int) -> c_int;

    DH_new() -> Ptr;
    DH_free(Ptr);
    DH_set0_pqg(Ptr, Ptr, Ptr, Ptr) -> c_int;
    DH_set0_key(Ptr, Ptr, Ptr) -> c_int;
    DH_get0_key(CPtr, *mut CPtr, *mut CPtr);
    DH_get0_pqg(CPtr, *mut CPtr, *mut CPtr, *mut CPtr);
    DH_generate_key(Ptr) -> c_int;
    DH_generate_parameters_ex(Ptr, c_int, c_int, Ptr) -> c_int;
    DH_check(CPtr, *mut c_int) -> c_int;
    DH_check_pub_key(CPtr, CPtr, *mut c_int) -> c_int;
    DH_compute_key(*mut u8, CPtr, Ptr) -> c_int;
    DH_size(CPtr) -> c_int;
}
