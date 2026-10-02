//! The OpenSSL 3 backend. `libcrypto` is loaded at runtime by `lumen_os::dynlib` (the loader
//! `lumen-tls` uses too) and driven through the EVP interfaces (`EVP_PKEY_encrypt`, `sign`,
//! `verify`, `keygen`, keys built with `EVP_PKEY_fromdata`) and the `BN_*` / `DH_*` functions Node
//! itself uses, so semantics and error texts are Node's. Errors come off OpenSSL's error queue.

mod api;

use std::ffi::CString;
use std::os::raw::{c_char, c_int};
use std::sync::OnceLock;

use api::{Api, CPtr, CryptOp, Free, Ptr};

use crate::error::{CryptoError, Result};
use crate::{
    pad_be, Algo, Backend, DhParams, DsaParams, DsaPrivateKey, DsaPublicKey, PssSalt, RsaPadding, RsaPrivateKey, RsaPublicKey,
    RsaScheme,
};

const RSA_PKCS1_PADDING: c_int = 1;
const RSA_NO_PADDING: c_int = 3;
const RSA_PKCS1_OAEP_PADDING: c_int = 4;
const RSA_PKCS1_PSS_PADDING: c_int = 6;
const PSS_SALTLEN_DIGEST: c_int = -1;
const PSS_SALTLEN_AUTO: c_int = -2;
const SELECT_PUBLIC_KEY: c_int = 0x86;
const SELECT_KEYPAIR: c_int = 0x87;
const BN_FLG_CONSTTIME: c_int = 0x04;
const FILE: &[u8] = b"lumen-crypto\0";

struct OpenSsl {
    api: Api,
}

pub(crate) fn backend() -> std::result::Result<&'static dyn Backend, &'static str> {
    static BACKEND: OnceLock<std::result::Result<OpenSsl, String>> = OnceLock::new();
    let loaded = BACKEND.get_or_init(|| {
        let lib = lumen_os::dynlib::openssl_crypto().map_err(str::to_string)?;
        let api = Api::load(lib)?;
        if unsafe { (api.OPENSSL_version_major)() } < 3 {
            return Err("OpenSSL 3 is required".to_string());
        }
        Ok(OpenSsl { api })
    });
    match loaded {
        Ok(backend) => Ok(backend),
        Err(reason) => Err(reason.as_str()),
    }
}

/// An owned OpenSSL object, freed on drop.
struct Handle {
    ptr: Ptr,
    free: Free,
}

impl Handle {
    fn ptr(&self) -> Ptr {
        self.ptr
    }

    /// Hands ownership to OpenSSL (`DH_set0_*` and friends).
    fn release(self) -> Ptr {
        let ptr = self.ptr;
        std::mem::forget(self);
        ptr
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { (self.free)(self.ptr) }
    }
}

fn c_name(name: &str) -> CString {
    CString::new(name).expect("names have no NUL")
}

fn c_int_len(len: usize) -> Result<c_int> {
    c_int::try_from(len).map_err(|_| CryptoError::failed("Value too large"))
}

impl OpenSsl {
    /// The oldest error of the thread's queue as a [`CryptoError`] (what Node reports), clearing the
    /// queue; `fallback` when OpenSSL left nothing.
    fn take_error(&self, fallback: &str) -> CryptoError {
        unsafe {
            let code = (self.api.ERR_get_error)();
            (self.api.ERR_clear_error)();
            if code == 0 {
                return CryptoError::failed(fallback);
            }
            let text = |p: *const c_char| {
                if p.is_null() {
                    None
                } else {
                    Some(std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned())
                }
            };
            let library = text((self.api.ERR_lib_error_string)(code)).unwrap_or_default();
            let reason = text((self.api.ERR_reason_error_string)(code)).unwrap_or_else(|| format!("reason({})", code & 0x7f_ffff));
            CryptoError::from_queue_entry(code as u32, &library, &reason)
        }
    }

    fn clear_errors(&self) {
        unsafe { (self.api.ERR_clear_error)() }
    }

    fn check(&self, ret: c_int, what: &str) -> Result<()> {
        if ret > 0 {
            Ok(())
        } else {
            Err(self.take_error(what))
        }
    }

    fn handle(&self, ptr: Ptr, free: Free, what: &str) -> Result<Handle> {
        if ptr.is_null() {
            Err(self.take_error(what))
        } else {
            Ok(Handle { ptr, free })
        }
    }

    fn bn(&self, bytes: &[u8]) -> Result<Handle> {
        let ptr = unsafe { (self.api.BN_bin2bn)(bytes.as_ptr(), c_int_len(bytes.len())?, std::ptr::null_mut()) };
        self.handle(ptr, self.api.BN_free, "BN_bin2bn failed")
    }

    fn bn_bytes(&self, bn: CPtr) -> Vec<u8> {
        unsafe {
            let bytes = ((self.api.BN_num_bits)(bn) as usize).div_ceil(8);
            let mut out = vec![0u8; bytes];
            (self.api.BN_bn2bin)(bn, out.as_mut_ptr());
            out
        }
    }

    fn md(&self, algo: Algo) -> Result<CPtr> {
        let name = match algo {
            Algo::Md5 => "MD5",
            Algo::Sha1 => "SHA1",
            Algo::Sha224 => "SHA224",
            Algo::Sha256 => "SHA256",
            Algo::Sha384 => "SHA384",
            Algo::Sha512 => "SHA512",
            Algo::Sha512_224 => "SHA512-224",
            Algo::Sha512_256 => "SHA512-256",
            Algo::Sha3_224 => "SHA3-224",
            Algo::Sha3_256 => "SHA3-256",
            Algo::Sha3_384 => "SHA3-384",
            Algo::Sha3_512 => "SHA3-512",
            Algo::Shake128 => "SHAKE128",
            Algo::Shake256 => "SHAKE256",
            Algo::Ripemd160 => "RIPEMD160",
            Algo::Blake2b512 => "BLAKE2B512",
            Algo::Blake2s256 => "BLAKE2S256",
            Algo::Sm3 => "SM3",
            Algo::Md5Sha1 => "MD5-SHA1",
        };
        let name = c_name(name);
        let md = unsafe { (self.api.EVP_get_digestbyname)(name.as_ptr()) };
        if md.is_null() {
            self.clear_errors();
            return Err(CryptoError::error("Digest method not supported").with_code("ERR_OSSL_EVP_UNSUPPORTED"));
        }
        Ok(md)
    }

    /// An `EVP_PKEY` of `kind` (`RSA`, `DSA`) built from named integer components.
    fn pkey(&self, kind: &str, selection: c_int, components: &[(&str, &[u8])]) -> Result<Handle> {
        unsafe {
            let builder = self.handle((self.api.OSSL_PARAM_BLD_new)(), self.api.OSSL_PARAM_BLD_free, "OSSL_PARAM_BLD_new failed")?;
            // The builder keeps pointers to the names and numbers until `to_param`.
            let mut numbers = Vec::new();
            let mut names = Vec::new();
            for (name, bytes) in components {
                let bn = self.bn(bytes)?;
                let key = c_name(name);
                self.check((self.api.OSSL_PARAM_BLD_push_BN)(builder.ptr(), key.as_ptr(), bn.ptr()), "OSSL_PARAM_BLD_push_BN failed")?;
                numbers.push(bn);
                names.push(key);
            }
            let params = self.handle((self.api.OSSL_PARAM_BLD_to_param)(builder.ptr()), self.api.OSSL_PARAM_free, "OSSL_PARAM_BLD_to_param failed")?;
            let kind = c_name(kind);
            let ctx = self.handle(
                (self.api.EVP_PKEY_CTX_new_from_name)(std::ptr::null_mut(), kind.as_ptr(), std::ptr::null()),
                self.api.EVP_PKEY_CTX_free,
                "EVP_PKEY_CTX_new_from_name failed",
            )?;
            self.check((self.api.EVP_PKEY_fromdata_init)(ctx.ptr()), "EVP_PKEY_fromdata_init failed")?;
            let mut pkey: Ptr = std::ptr::null_mut();
            self.check((self.api.EVP_PKEY_fromdata)(ctx.ptr(), &mut pkey, selection, params.ptr()), "EVP_PKEY_fromdata failed")?;
            Ok(Handle { ptr: pkey, free: self.api.EVP_PKEY_free })
        }
    }

    fn pkey_bn(&self, pkey: &Handle, name: &str) -> Result<Vec<u8>> {
        unsafe {
            let key = c_name(name);
            let mut bn: Ptr = std::ptr::null_mut();
            self.check((self.api.EVP_PKEY_get_bn_param)(pkey.ptr(), key.as_ptr(), &mut bn), "EVP_PKEY_get_bn_param failed")?;
            let bn = Handle { ptr: bn, free: self.api.BN_free };
            Ok(self.bn_bytes(bn.ptr()))
        }
    }

    fn rsa_public_pkey(&self, k: &RsaPublicKey) -> Result<Handle> {
        self.pkey("RSA", SELECT_PUBLIC_KEY, &[("n", &k.n), ("e", &k.e)])
    }

    fn rsa_private_pkey(&self, k: &RsaPrivateKey) -> Result<Handle> {
        self.pkey(
            "RSA",
            SELECT_KEYPAIR,
            &[
                ("n", &k.public.n),
                ("e", &k.public.e),
                ("d", &k.d),
                ("rsa-factor1", &k.p),
                ("rsa-factor2", &k.q),
                ("rsa-exponent1", &k.dp),
                ("rsa-exponent2", &k.dq),
                ("rsa-coefficient1", &k.qi),
            ],
        )
    }

    fn dsa_params_components<'a>(&self, params: &'a DsaParams) -> [(&'static str, &'a [u8]); 3] {
        [("p", &params.p), ("q", &params.q), ("g", &params.g)]
    }

    fn dsa_public_pkey(&self, k: &DsaPublicKey) -> Result<Handle> {
        let [p, q, g] = self.dsa_params_components(&k.params);
        self.pkey("DSA", SELECT_PUBLIC_KEY, &[p, q, g, ("pub", &k.y)])
    }

    fn dsa_private_pkey(&self, k: &DsaPrivateKey) -> Result<Handle> {
        let [p, q, g] = self.dsa_params_components(&k.public.params);
        self.pkey("DSA", SELECT_KEYPAIR, &[p, q, g, ("pub", &k.public.y), ("priv", &k.x)])
    }

    fn pkey_ctx(&self, pkey: &Handle) -> Result<Handle> {
        self.handle(
            unsafe { (self.api.EVP_PKEY_CTX_new)(pkey.ptr(), std::ptr::null_mut()) },
            self.api.EVP_PKEY_CTX_free,
            "EVP_PKEY_CTX_new failed",
        )
    }

    fn set_padding(&self, ctx: &Handle, padding: c_int) -> Result<()> {
        self.check(unsafe { (self.api.EVP_PKEY_CTX_set_rsa_padding)(ctx.ptr(), padding) }, "Failed to set RSA padding")
    }

    fn configure_padding(&self, ctx: &Handle, padding: &RsaPadding) -> Result<()> {
        match padding {
            RsaPadding::Pkcs1 => self.set_padding(ctx, RSA_PKCS1_PADDING),
            RsaPadding::None => self.set_padding(ctx, RSA_NO_PADDING),
            RsaPadding::Oaep { hash, mgf1, label } => {
                self.set_padding(ctx, RSA_PKCS1_OAEP_PADDING)?;
                unsafe {
                    self.check((self.api.EVP_PKEY_CTX_set_rsa_oaep_md)(ctx.ptr(), self.md(*hash)?), "Failed to set OAEP digest")?;
                    self.check((self.api.EVP_PKEY_CTX_set_rsa_mgf1_md)(ctx.ptr(), self.md(*mgf1)?), "Failed to set MGF1 digest")?;
                    if !label.is_empty() {
                        let buffer = (self.api.CRYPTO_malloc)(label.len(), FILE.as_ptr().cast(), 0);
                        if buffer.is_null() {
                            return Err(self.take_error("Out of memory"));
                        }
                        std::ptr::copy_nonoverlapping(label.as_ptr(), buffer.cast::<u8>(), label.len());
                        let ret = (self.api.EVP_PKEY_CTX_set0_rsa_oaep_label)(ctx.ptr(), buffer, c_int_len(label.len())?);
                        if ret <= 0 {
                            (self.api.CRYPTO_free)(buffer, FILE.as_ptr().cast(), 0);
                            return Err(self.take_error("Failed to set OAEP label"));
                        }
                    }
                }
                Ok(())
            }
        }
    }

    /// The two-call (size, then data) form of the `EVP_PKEY_*` operations that transform a buffer.
    fn transform(&self, ctx: &Handle, op: CryptOp, input: &[u8]) -> Result<Vec<u8>> {
        unsafe {
            let mut len = 0usize;
            self.check(op(ctx.ptr(), std::ptr::null_mut(), &mut len, input.as_ptr(), input.len()), "RSA operation failed")?;
            let mut out = vec![0u8; len];
            self.check(op(ctx.ptr(), out.as_mut_ptr(), &mut len, input.as_ptr(), input.len()), "RSA operation failed")?;
            out.truncate(len);
            Ok(out)
        }
    }

    fn rsa_crypt(
        &self,
        pkey: &Handle,
        init: unsafe extern "C" fn(Ptr) -> c_int,
        op: CryptOp,
        padding: &RsaPadding,
        input: &[u8],
    ) -> Result<Vec<u8>> {
        self.clear_errors();
        let ctx = self.pkey_ctx(pkey)?;
        self.check(unsafe { init(ctx.ptr()) }, "RSA operation failed")?;
        self.configure_padding(&ctx, padding)?;
        self.transform(&ctx, op, input)
    }

    fn scheme_ctx(&self, pkey: &Handle, init: unsafe extern "C" fn(Ptr) -> c_int, scheme: &RsaScheme) -> Result<Handle> {
        let ctx = self.pkey_ctx(pkey)?;
        unsafe {
            self.check(init(ctx.ptr()), "RSA operation failed")?;
            match scheme {
                RsaScheme::Pkcs1 { hash } => {
                    self.set_padding(&ctx, RSA_PKCS1_PADDING)?;
                    self.check((self.api.EVP_PKEY_CTX_set_signature_md)(ctx.ptr(), self.md(*hash)?), "Failed to set digest")?;
                }
                RsaScheme::Pss { hash, mgf1, salt } => {
                    self.set_padding(&ctx, RSA_PKCS1_PSS_PADDING)?;
                    self.check((self.api.EVP_PKEY_CTX_set_signature_md)(ctx.ptr(), self.md(*hash)?), "Failed to set digest")?;
                    self.check((self.api.EVP_PKEY_CTX_set_rsa_mgf1_md)(ctx.ptr(), self.md(*mgf1)?), "Failed to set MGF1 digest")?;
                    let salt = match salt {
                        PssSalt::Digest => PSS_SALTLEN_DIGEST,
                        PssSalt::MaxOrAuto => PSS_SALTLEN_AUTO,
                        PssSalt::Length(n) => c_int_len(*n as usize)?,
                    };
                    self.check((self.api.EVP_PKEY_CTX_set_rsa_pss_saltlen)(ctx.ptr(), salt), "Failed to set salt length")?;
                }
            }
        }
        Ok(ctx)
    }

    fn dh(&self, params: &DhParams, private: Option<&[u8]>) -> Result<Handle> {
        unsafe {
            let dh = self.handle((self.api.DH_new)(), self.api.DH_free, "DH_new failed")?;
            let p = self.bn(&params.p)?;
            let g = self.bn(&params.g)?;
            let (p_ptr, g_ptr) = (p.release(), g.release());
            if (self.api.DH_set0_pqg)(dh.ptr(), p_ptr, std::ptr::null_mut(), g_ptr) != 1 {
                (self.api.BN_free)(p_ptr);
                (self.api.BN_free)(g_ptr);
                return Err(self.take_error("DH_set0_pqg failed"));
            }
            if let Some(private) = private {
                let x = self.bn(private)?.release();
                if (self.api.DH_set0_key)(dh.ptr(), std::ptr::null_mut(), x) != 1 {
                    (self.api.BN_free)(x);
                    return Err(self.take_error("DH_set0_key failed"));
                }
            }
            Ok(dh)
        }
    }
}

impl Backend for OpenSsl {
    fn name(&self) -> &'static str {
        "openssl"
    }

    fn rsa_encrypt(&self, key: &RsaPublicKey, padding: &RsaPadding, input: &[u8]) -> Result<Vec<u8>> {
        let pkey = self.rsa_public_pkey(key)?;
        self.rsa_crypt(&pkey, self.api.EVP_PKEY_encrypt_init, self.api.EVP_PKEY_encrypt, padding, input)
    }

    fn rsa_decrypt(&self, key: &RsaPrivateKey, padding: &RsaPadding, input: &[u8]) -> Result<Vec<u8>> {
        let pkey = self.rsa_private_pkey(key)?;
        self.rsa_crypt(&pkey, self.api.EVP_PKEY_decrypt_init, self.api.EVP_PKEY_decrypt, padding, input)
    }

    fn rsa_private_encrypt(&self, key: &RsaPrivateKey, padding: &RsaPadding, input: &[u8]) -> Result<Vec<u8>> {
        let pkey = self.rsa_private_pkey(key)?;
        self.rsa_crypt(&pkey, self.api.EVP_PKEY_sign_init, self.api.EVP_PKEY_sign, padding, input)
    }

    fn rsa_public_decrypt(&self, key: &RsaPublicKey, padding: &RsaPadding, input: &[u8]) -> Result<Vec<u8>> {
        let pkey = self.rsa_public_pkey(key)?;
        self.rsa_crypt(&pkey, self.api.EVP_PKEY_verify_recover_init, self.api.EVP_PKEY_verify_recover, padding, input)
    }

    fn rsa_sign(&self, key: &RsaPrivateKey, scheme: &RsaScheme, hashed: &[u8]) -> Result<Vec<u8>> {
        self.clear_errors();
        let pkey = self.rsa_private_pkey(key)?;
        let ctx = self.scheme_ctx(&pkey, self.api.EVP_PKEY_sign_init, scheme)?;
        self.transform(&ctx, self.api.EVP_PKEY_sign, hashed)
    }

    fn rsa_verify(&self, key: &RsaPublicKey, scheme: &RsaScheme, hashed: &[u8], signature: &[u8]) -> Result<bool> {
        self.clear_errors();
        let pkey = self.rsa_public_pkey(key)?;
        let ctx = self.scheme_ctx(&pkey, self.api.EVP_PKEY_verify_init, scheme)?;
        let ret = unsafe { (self.api.EVP_PKEY_verify)(ctx.ptr(), signature.as_ptr(), signature.len(), hashed.as_ptr(), hashed.len()) };
        self.clear_errors();
        Ok(ret == 1)
    }

    fn rsa_generate(&self, bits: u32, e: &[u8]) -> Result<RsaPrivateKey> {
        self.clear_errors();
        unsafe {
            let kind = c_name("RSA");
            let ctx = self.handle(
                (self.api.EVP_PKEY_CTX_new_from_name)(std::ptr::null_mut(), kind.as_ptr(), std::ptr::null()),
                self.api.EVP_PKEY_CTX_free,
                "EVP_PKEY_CTX_new_from_name failed",
            )?;
            self.check((self.api.EVP_PKEY_keygen_init)(ctx.ptr()), "Key generation failed")?;
            self.check((self.api.EVP_PKEY_CTX_set_rsa_keygen_bits)(ctx.ptr(), c_int_len(bits as usize)?), "Key generation failed")?;
            let exponent = self.bn(e)?;
            self.check((self.api.EVP_PKEY_CTX_set1_rsa_keygen_pubexp)(ctx.ptr(), exponent.ptr()), "Key generation failed")?;
            let mut pkey: Ptr = std::ptr::null_mut();
            self.check((self.api.EVP_PKEY_keygen)(ctx.ptr(), &mut pkey), "Key generation failed")?;
            let pkey = Handle { ptr: pkey, free: self.api.EVP_PKEY_free };
            Ok(RsaPrivateKey {
                public: RsaPublicKey { n: self.pkey_bn(&pkey, "n")?, e: self.pkey_bn(&pkey, "e")? },
                d: self.pkey_bn(&pkey, "d")?,
                p: self.pkey_bn(&pkey, "rsa-factor1")?,
                q: self.pkey_bn(&pkey, "rsa-factor2")?,
                dp: self.pkey_bn(&pkey, "rsa-exponent1")?,
                dq: self.pkey_bn(&pkey, "rsa-exponent2")?,
                qi: self.pkey_bn(&pkey, "rsa-coefficient1")?,
            })
        }
    }

    fn dsa_sign(&self, key: &DsaPrivateKey, hashed: &[u8]) -> Result<Vec<u8>> {
        self.clear_errors();
        let pkey = self.dsa_private_pkey(key)?;
        let ctx = self.pkey_ctx(&pkey)?;
        self.check(unsafe { (self.api.EVP_PKEY_sign_init)(ctx.ptr()) }, "Failed to sign")?;
        self.transform(&ctx, self.api.EVP_PKEY_sign, hashed)
    }

    fn dsa_verify(&self, key: &DsaPublicKey, hashed: &[u8], signature: &[u8]) -> Result<bool> {
        self.clear_errors();
        let pkey = self.dsa_public_pkey(key)?;
        let ctx = self.pkey_ctx(&pkey)?;
        self.check(unsafe { (self.api.EVP_PKEY_verify_init)(ctx.ptr()) }, "Failed to verify")?;
        let ret = unsafe { (self.api.EVP_PKEY_verify)(ctx.ptr(), signature.as_ptr(), signature.len(), hashed.as_ptr(), hashed.len()) };
        self.clear_errors();
        Ok(ret == 1)
    }

    fn dsa_generate(&self, bits: u32, divisor_bits: Option<u32>) -> Result<DsaPrivateKey> {
        self.clear_errors();
        unsafe {
            let kind = c_name("DSA");
            let ctx = self.handle(
                (self.api.EVP_PKEY_CTX_new_from_name)(std::ptr::null_mut(), kind.as_ptr(), std::ptr::null()),
                self.api.EVP_PKEY_CTX_free,
                "EVP_PKEY_CTX_new_from_name failed",
            )?;
            self.check((self.api.EVP_PKEY_paramgen_init)(ctx.ptr()), "DSA parameter generation failed")?;
            self.check((self.api.EVP_PKEY_CTX_set_dsa_paramgen_bits)(ctx.ptr(), c_int_len(bits as usize)?), "DSA parameter generation failed")?;
            if let Some(q_bits) = divisor_bits {
                self.check((self.api.EVP_PKEY_CTX_set_dsa_paramgen_q_bits)(ctx.ptr(), c_int_len(q_bits as usize)?), "DSA parameter generation failed")?;
            }
            let mut params: Ptr = std::ptr::null_mut();
            self.check((self.api.EVP_PKEY_paramgen)(ctx.ptr(), &mut params), "DSA parameter generation failed")?;
            let params = Handle { ptr: params, free: self.api.EVP_PKEY_free };
            let key_ctx = self.pkey_ctx(&params)?;
            self.check((self.api.EVP_PKEY_keygen_init)(key_ctx.ptr()), "Key generation failed")?;
            let mut pkey: Ptr = std::ptr::null_mut();
            self.check((self.api.EVP_PKEY_keygen)(key_ctx.ptr(), &mut pkey), "Key generation failed")?;
            let pkey = Handle { ptr: pkey, free: self.api.EVP_PKEY_free };
            Ok(DsaPrivateKey {
                public: DsaPublicKey {
                    params: DsaParams { p: self.pkey_bn(&pkey, "p")?, q: self.pkey_bn(&pkey, "q")?, g: self.pkey_bn(&pkey, "g")? },
                    y: self.pkey_bn(&pkey, "pub")?,
                },
                x: self.pkey_bn(&pkey, "priv")?,
            })
        }
    }

    fn dh_check(&self, params: &DhParams) -> Result<u32> {
        self.clear_errors();
        let dh = self.dh(params, None)?;
        let mut codes: c_int = 0;
        self.check(unsafe { (self.api.DH_check)(dh.ptr(), &mut codes) }, "Unspecified validation error")?;
        Ok(codes as u32)
    }

    fn dh_check_public(&self, params: &DhParams, public: &[u8]) -> Result<u32> {
        self.clear_errors();
        let dh = self.dh(params, None)?;
        let public = self.bn(public)?;
        let mut codes: c_int = 0;
        self.check(unsafe { (self.api.DH_check_pub_key)(dh.ptr(), public.ptr(), &mut codes) }, "Unspecified validation error")?;
        Ok(codes as u32)
    }

    fn dh_generate_prime(&self, bits: u32, generator: u32) -> Result<Vec<u8>> {
        self.clear_errors();
        unsafe {
            let dh = self.handle((self.api.DH_new)(), self.api.DH_free, "DH_new failed")?;
            let generator = if generator <= 1 { 2 } else { generator };
            self.check(
                (self.api.DH_generate_parameters_ex)(dh.ptr(), c_int_len(bits as usize)?, c_int_len(generator as usize)?, std::ptr::null_mut()),
                "Key generation failed",
            )?;
            let mut p: CPtr = std::ptr::null();
            (self.api.DH_get0_pqg)(dh.ptr(), &mut p, std::ptr::null_mut(), std::ptr::null_mut());
            Ok(self.bn_bytes(p))
        }
    }

    fn dh_generate_key(&self, params: &DhParams, private: Option<&[u8]>) -> Result<(Vec<u8>, Vec<u8>)> {
        self.clear_errors();
        let dh = self.dh(params, private)?;
        unsafe {
            self.check((self.api.DH_generate_key)(dh.ptr()), "Key generation failed")?;
            let (mut public, mut private): (CPtr, CPtr) = (std::ptr::null(), std::ptr::null());
            (self.api.DH_get0_key)(dh.ptr(), &mut public, &mut private);
            Ok((self.bn_bytes(private), self.bn_bytes(public)))
        }
    }

    fn dh_public(&self, params: &DhParams, private: &[u8]) -> Result<Vec<u8>> {
        self.clear_errors();
        unsafe {
            let (g, x, p) = (self.bn(&params.g)?, self.bn(private)?, self.bn(&params.p)?);
            (self.api.BN_set_flags)(x.ptr(), BN_FLG_CONSTTIME);
            let ctx = self.handle((self.api.BN_CTX_new)(), self.api.BN_CTX_free, "BN_CTX_new failed")?;
            let out = self.handle((self.api.BN_new)(), self.api.BN_free, "BN_new failed")?;
            self.check((self.api.BN_mod_exp)(out.ptr(), g.ptr(), x.ptr(), p.ptr(), ctx.ptr()), "Key generation failed")?;
            Ok(self.bn_bytes(out.ptr()))
        }
    }

    fn dh_compute(&self, params: &DhParams, private: &[u8], peer: &[u8]) -> Result<Vec<u8>> {
        self.clear_errors();
        let dh = self.dh(params, Some(private))?;
        let peer = self.bn(peer)?;
        unsafe {
            let size = (self.api.DH_size)(dh.ptr());
            if size <= 0 {
                return Err(self.take_error("Failed to compute DH key"));
            }
            let mut out = vec![0u8; size as usize];
            let written = (self.api.DH_compute_key)(out.as_mut_ptr(), peer.ptr(), dh.ptr());
            if written < 0 {
                return Err(self.take_error("Failed to compute DH key"));
            }
            out.truncate(written as usize);
            Ok(pad_be(&out, size as usize))
        }
    }

    fn prime_generate(&self, bits: u32, safe: bool, add: Option<&[u8]>, rem: Option<&[u8]>) -> Result<Vec<u8>> {
        self.clear_errors();
        unsafe {
            let prime = self.handle((self.api.BN_new)(), self.api.BN_free, "BN_new failed")?;
            let add = add.map(|a| self.bn(a)).transpose()?;
            let rem = rem.map(|r| self.bn(r)).transpose()?;
            let ptr = |h: &Option<Handle>| h.as_ref().map_or(std::ptr::null(), |h| h.ptr() as CPtr);
            self.check(
                (self.api.BN_generate_prime_ex)(prime.ptr(), c_int_len(bits as usize)?, safe as c_int, ptr(&add), ptr(&rem), std::ptr::null_mut()),
                "Prime generation failed",
            )?;
            Ok(self.bn_bytes(prime.ptr()))
        }
    }

    fn prime_check(&self, candidate: &[u8], checks: u32) -> Result<bool> {
        self.clear_errors();
        unsafe {
            let candidate = self.bn(candidate)?;
            let ctx = self.handle((self.api.BN_CTX_new)(), self.api.BN_CTX_free, "BN_CTX_new failed")?;
            let ret = (self.api.BN_is_prime_ex)(candidate.ptr(), c_int_len(checks as usize)?, ctx.ptr(), std::ptr::null_mut());
            if ret < 0 {
                return Err(self.take_error("Primality check failed"));
            }
            Ok(ret == 1)
        }
    }
}
